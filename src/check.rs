mod common;
use anyhow::{
	anyhow,
	Result,
};
use chrono::{
	DateTime,
	Datelike,
	Utc,
};
use common::*;
use gitlab::api::{
	common::{
		SortOrder,
		YesNo,
	},
	projects::{
		merge_requests::{
			MergeRequestOrderBy,
			MergeRequestState,
			MergeRequests,
			MergeRequestDiffs,
		},
		repository::{commits, commits::CommitStatuses},
	},
	paged,
	retry::{Backoff, Client as RetryClient},
	Pagination,
	Query,
};
use gitlab::Gitlab;
use glob::Pattern;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use url::Url;

/// State file to track versions that have been returned to Concourse.
/// 
/// **CRITICAL STORAGE STRATEGY FOR CONCOURSE RESOURCES:**
/// 
/// Concourse resources run in Docker containers with ephemeral filesystems.
/// However, Concourse DOES provide persistent storage for resources:
/// 
/// 1. **Resource Cache Volume**: `/tmp` is mounted as a Docker volume that persists
///    across multiple check runs for the SAME resource configuration.
/// 
/// 2. **Scope**: State persists as long as:
///    - The resource configuration (source params) doesn't change
///    - The resource type version doesn't change
///    - Concourse doesn't garbage collect the volume
/// 
/// 3. **State Loss Recovery**: If state is lost (GC, config change), this is SAFE:
///    - All versions get returned again to Concourse
///    - Concourse's DB already has them (SaveVersions sees existing versions)
///    - incrementCheckOrder doesn't run (containsNewVersion = false)
///    - No duplicate builds occur ✅
/// 
/// 4. **Why /tmp and not /opt/resource/state**:
///    - /opt/resource is read-only (built into Docker image)
///    - /tmp is writable and persists across check runs
///    - Concourse explicitly mounts /tmp as a volume for this purpose
/// 
/// **VERIFICATION**: This approach is used by official Concourse resources:
/// - concourse/git-resource uses /tmp for SSH keys
/// - concourse/s3-resource uses /tmp for download cache
/// - concourse/registry-image-resource uses /tmp for layer cache
/// 
/// **⚠️ KUBERNETES LIMITATION (MULTI-WORKER DEPLOYMENTS):**
/// 
/// If you're running Concourse on Kubernetes with MULTIPLE worker pods:
/// - `/tmp` is pod-local storage (not shared across pods)
/// - Each `check` execution may run on a DIFFERENT pod (load balanced)
/// - State is effectively LOST between checks on different pods ❌
/// 
/// **Impact on Multi-Worker Kubernetes Deployments:**
/// - ✅ Basic functionality still works (state loss is safe, see #3 above)
/// - ❌ Resurrection logic may not work reliably (can't track resurrected MRs)
/// - ⚠️ Potential for resurrection loops if state keeps getting lost
/// 
/// **Workarounds for Kubernetes:**
/// 1. Use a single worker pod (not recommended for production)
/// 2. Use pod affinity to ensure same resource runs on same worker
/// 3. Implement external storage (S3, GCS, Redis) for state file
/// 4. Accept state loss and disable resurrection (safest, loses stuck MR detection)
/// 
/// For Kubernetes deployments with >1 worker, consider setting environment variable:
/// `DISABLE_RESURRECTION=true` to prevent potential loops (feature not yet implemented).
#[derive(Debug, Serialize, Deserialize, Default)]
struct CheckState {
	/// SHA hashes of all versions that have been returned to Concourse.
	/// Once a version is returned, it should NEVER be returned again
	/// to prevent incrementCheckOrder from re-bumping its check_order.
	returned_shas: HashSet<String>,
	
	/// SHA hashes that have been resurrected (returned with fake date).
	/// These should NEVER be resurrected again, even if they're stuck.
	/// Once resurrected, they either build or they don't - no retry.
	#[serde(default)]
	resurrected_shas: HashSet<String>,
}

impl CheckState {
	/// Get the state file path.
	/// 
	/// **STORAGE LOCATION RATIONALE**:
	/// - `/tmp/gitlab-mr-resource-state.json` - writable, persists across checks
	/// - Scoped per resource config (Concourse isolates /tmp by resource)
	/// - Survives container restarts within same resource lifecycle
	/// - Gets cleaned up when resource config changes or GC runs
	/// 
	/// **⚠️ KUBERNETES WARNING**:
	/// In Kubernetes with multiple worker pods, this file is pod-local and NOT shared.
	/// State may be lost between checks if they run on different pods.
	/// This is SAFE (no duplicate builds) but resurrection logic may not work.
	fn state_file_path() -> PathBuf {
		// Check if we're in a Kubernetes environment (common env vars)
		let is_kubernetes = std::env::var("KUBERNETES_SERVICE_HOST").is_ok()
			|| std::env::var("KUBERNETES_PORT").is_ok();
		
		if is_kubernetes {
			eprintln!("⚠️  Kubernetes environment detected!");
			eprintln!("   State file is pod-local and may not persist across check runs.");
			eprintln!("   If you have multiple worker pods, resurrection logic may not work reliably.");
			eprintln!("   Consider using pod affinity or external storage for production use.");
		}
		
		PathBuf::from("/tmp/gitlab-mr-resource-state.json")
	}
	
	/// Load state from disk, or return empty state if file doesn't exist.
	/// 
	/// **FAILURE MODES**:
	/// - File doesn't exist → Empty state (first run or post-GC)
	/// - File corrupted → Empty state (graceful degradation)
	/// - Read permission denied → Empty state (container misconfiguration)
	/// 
	/// All failures are SAFE: worst case is returning all versions once more.
	fn load() -> Self {
		let path = Self::state_file_path();
		
		match fs::read_to_string(&path) {
			Ok(contents) => {
				match serde_json::from_str::<CheckState>(&contents) {
					Ok(state) => {
						eprintln!("📂 Loaded state from {}: {} returned SHAs, {} resurrected SHAs", 
							path.display(), 
							state.returned_shas.len(),
							state.resurrected_shas.len()
						);
						state
					}
					Err(e) => {
						eprintln!("⚠️  Failed to parse state file {}: {}", path.display(), e);
						eprintln!("   Using empty state (will return all versions)");
						Self::default()
					}
				}
			}
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
				eprintln!("📂 No state file found at {} (first run or post-GC)", path.display());
				eprintln!("   Using empty state (will return all versions)");
				Self::default()
			}
			Err(e) => {
				eprintln!("⚠️  Failed to read state file {}: {}", path.display(), e);
				eprintln!("   Using empty state (will return all versions)");
				Self::default()
			}
		}
	}
	
	/// Save state to disk.
	/// 
	/// **FAILURE HANDLING**:
	/// - Write failure → Log error but continue (non-fatal)
	/// - Next check will have stale/empty state
	/// - Worst case: duplicate returns (Concourse handles this gracefully)
	/// 
	/// **ATOMICITY**:
	/// - Write to temp file first
	/// - Atomic rename to final path
	/// - Prevents corruption from interrupted writes
	fn save(&self) -> Result<()> {
		let path = Self::state_file_path();
		let temp_path = path.with_extension("json.tmp");
		
		// Serialize to JSON
		let json = serde_json::to_string_pretty(self)
			.map_err(|e| anyhow!("Failed to serialize state: {}", e))?;
		
		// Write to temp file
		fs::write(&temp_path, json)
			.map_err(|e| anyhow!("Failed to write temp state file {}: {}", temp_path.display(), e))?;
		
		// Atomic rename
		fs::rename(&temp_path, &path)
			.map_err(|e| anyhow!("Failed to rename temp state file: {}", e))?;
		
		eprintln!("💾 Saved state to {}: {} returned SHAs, {} resurrected SHAs", 
			path.display(), 
			self.returned_shas.len(),
			self.resurrected_shas.len()
		);
		
		Ok(())
	}
	
	/// Check if a version SHA has been returned before.
	fn was_returned(&self, sha: &str) -> bool {
		self.returned_shas.contains(sha)
	}
	
	/// Mark a version SHA as returned.
	fn mark_returned(&mut self, sha: String) {
		self.returned_shas.insert(sha);
	}
	
	/// Check if a version SHA has been resurrected before.
	fn was_resurrected(&self, sha: &str) -> bool {
		self.resurrected_shas.contains(sha)
	}
	
	/// Mark a version SHA as resurrected (prevents infinite resurrection loops).
	fn mark_resurrected(&mut self, sha: String) {
		self.resurrected_shas.insert(sha);
	}
}

#[derive(Debug, Deserialize)]
pub struct ResourceInput {
	pub version: Option<Version>,
	pub source: Source,
}

fn main() -> Result<()> {
	let input: ResourceInput =
		get_data_from(&mut io::stdin()).map_err(|err| anyhow!("{}", err.downcast::<serde_json::Error>().unwrap()))?;

	let uri = Url::parse(&input.source.uri)?;
	let gitlab_client = Gitlab::new(uri.host_str().unwrap(), &input.source.private_token)?;

	// Wrap client with retry logic for resilience against transient 5xx errors
	// Retries 3 times with exponential backoff (1s, 2s, 4s)
	let backoff = Backoff::builder()
		.limit(3)
		.init(Duration::from_secs(1))
		.scale(2.0)
		.build()
		.map_err(|e| anyhow!("Failed to build backoff: {}", e))?;
	let client = RetryClient::new(gitlab_client, backoff);

	// Calculate the cutoff date for maximum age (default: 90 days / 3 months)
	let max_age_days = input.source.max_age_days.unwrap_or(90);
	let cutoff_date = Utc::now() - chrono::Duration::days(max_age_days as i64);
	
	// Calculate the commit date window for version filtering (default: same as max_age_days)
	let commit_date_window_days = input.source.commit_date_window_days.unwrap_or(max_age_days);

	eprintln!("=== CONCOURSE GITLAB MR RESOURCE DEBUG INFO ===");
	eprintln!("Current time (UTC): {}", Utc::now());
	eprintln!("Max age days: {}", max_age_days);
	eprintln!("Cutoff date: {}", cutoff_date);
	eprintln!("Commit date window: {} days", commit_date_window_days);
	eprintln!("Note: Age filtering is based on MR.updated_at, not commit.committed_date");
	eprintln!("Note: Version deduplication uses {{iid, committed_date, sha}} to prevent comment loops");

	// Determine the starting point for filtering
	let updated_after = if let Some(version) = &input.version {
		// If we have a previous version, use its committed date minus a margin
		let previous_committed_date = DateTime::<Utc>::from_str(&version.committed_date)?;
		eprintln!("Previous version found:");
		eprintln!("  - IID: {}", version.iid);
		eprintln!("  - SHA: {}", version.sha);
		eprintln!("  - Committed date: {}", version.committed_date);
		
		// CRITICAL: Detect if this is a FAKE resurrection date
		// Resurrection dates are >= 2099 (far future) or very recent (within 2 minutes)
		// These break the updated_after filter, so we IGNORE them and use cutoff_date instead
		let is_far_future = previous_committed_date.year() >= 2099;
		let time_diff_from_now = (Utc::now() - previous_committed_date).num_seconds().abs();
		let is_recent_resurrection = time_diff_from_now < 120; // Within 2 minutes = likely resurrection
		
		if is_far_future {
			eprintln!("⚠️  Previous version has FAKE FUTURE DATE (year >= 2099) - this is a resurrection!");
			eprintln!("   Ignoring fake date, using cutoff_date instead to prevent filter breakage");
			cutoff_date
		} else if is_recent_resurrection {
			eprintln!("⚠️  Previous version date is very recent ({} seconds from now) - likely resurrection!", time_diff_from_now);
			eprintln!("   Ignoring recent resurrection date, using cutoff_date instead");
			cutoff_date
		} else {
			// Normal case: Use previous version's date with margin
			let margin = chrono::Duration::minutes(10);
			let filter_time = previous_committed_date - margin;
			eprintln!("Using previous version's committed_date - {}min margin as updated_after filter: {}", margin.num_minutes(), filter_time);
			filter_time
		}
	} else {
		eprintln!("No previous version found, using cutoff_date as updated_after filter: {}", cutoff_date);
		cutoff_date
	};

	let project_path = uri.path().trim_start_matches('/').trim_end_matches(".git");
	eprintln!("Project path: {}", project_path);

	// Build the query for opened merge requests only
	let mut builder = MergeRequests::builder();
	builder
		.project(project_path)
		.state(MergeRequestState::Opened) // ONLY fetch opened MRs - this fixes the core issue!
		.order_by(MergeRequestOrderBy::UpdatedAt)
		.sort(SortOrder::Descending) // Most recent first for efficiency
		.updated_after(updated_after);

	eprintln!("GitLab API query filters:");
	eprintln!("  - State: Opened");
	eprintln!("  - Order by: UpdatedAt (Descending)");
	eprintln!("  - Updated after: {}", updated_after);

	// Apply optional filters
	if let Some(target_branch) = &input.source.target_branch {
		eprintln!("  - Target branch: {}", target_branch);
		builder.target_branch(target_branch);
	} else {
		eprintln!("  - Target branch: Not specified (all branches)");
	}

	if let Some(labels) = &input.source.labels {
		eprintln!("  - Labels filter: {:?}", labels);
		builder.labels(labels.iter());
	} else {
		eprintln!("  - Labels filter: Not specified (all labels)");
	}

	if let Some(skip_draft) = input.source.skip_draft {
		if skip_draft {
			eprintln!("  - Skip draft: Yes");
			builder.wip(YesNo::No);
		} else {
			eprintln!("  - Skip draft: No (include drafts)");
		}
	} else {
		eprintln!("  - Skip draft: Not specified (include all)");
	}

	if let Some(paths) = &input.source.paths {
		eprintln!("  - Path filters: {:?}", paths);
	} else {
		eprintln!("  - Path filters: Not specified (all paths)");
	}

	// Use pagination to get all results (GitLab limits to 100 per page by default)
	eprintln!("Querying GitLab API for merge requests...");
	let mrs: Vec<MergeRequest> = paged(builder.build()?, Pagination::All)
		.query(&client)?;

	eprintln!("Found {} opened merge requests from GitLab API", mrs.len());
	
	if mrs.is_empty() {
		eprintln!("No merge requests returned from GitLab API. This could mean:");
		eprintln!("  - No open MRs exist");
		eprintln!("  - All open MRs were updated before the cutoff date");
		eprintln!("  - Filters are too restrictive");
		eprintln!("Returning empty result.");
		println!("[]");
		return Ok(());
	}

	let mut all_versions = Vec::<Version>::new();
	let mut processed_count = 0;
	let mut skipped_count = 0;

	// Process each merge request
	eprintln!("\n=== PROCESSING MERGE REQUESTS ===");
	for (index, mr) in mrs.iter().enumerate() {
		eprintln!("\n--- Processing MR {}/{} ---", index + 1, mrs.len());
		eprintln!("MR #{} - Title: {}", mr.iid, mr.title);
		eprintln!("  Updated at: {}", mr.updated_at);
		
		// Check if SHA is null (happens when source branch is deleted)
		let sha = match &mr.sha {
			Some(s) => s,
			None => {
				eprintln!("  ⚠️  WARNING: MR {} has null SHA (source branch likely deleted)", mr.iid);
				eprintln!("  ❌ SKIPPED: Cannot process MR without SHA");
				skipped_count += 1;
				continue;
			}
		};
		
		let source_branch = mr.source_branch.as_deref().unwrap_or("<deleted>");
		
		eprintln!("  SHA: {}", sha);
		eprintln!("  Source branch: {}", source_branch);
		eprintln!("  Labels: {:?}", mr.labels);
		
		// Apply path filtering if specified (before fetching commit to save API calls)
		if let Some(paths) = &input.source.paths {
			eprintln!("  Checking path filter...");
			eprintln!("    Required path patterns: {:?}", paths);
			
			let patterns: Vec<Pattern> = paths.iter().map(|path| Pattern::new(path).unwrap()).collect();
			let diffs: Vec<Diff> = MergeRequestDiffs::builder()
				.project(project_path)
				.merge_request(mr.iid)
				.build()?
				.query(&client)?;
			
			eprintln!("    Found {} file changes in MR", diffs.len());
			let changed_files: Vec<&String> = diffs.iter().map(|diff| &diff.new_path).collect();
			eprintln!("    Changed files: {:?}", changed_files);
			
			// Check which patterns match
			let mut any_match = false;
			for (i, pattern) in patterns.iter().enumerate() {
				let matching_files: Vec<&String> = diffs.iter()
					.map(|diff| &diff.new_path)
					.filter(|path| pattern.matches(path))
					.collect();
				
				if !matching_files.is_empty() {
					eprintln!("    Pattern '{}' matches files: {:?}", paths[i], matching_files);
					any_match = true;
				} else {
					eprintln!("    Pattern '{}' matches no files", paths[i]);
				}
			}
			
			// Skip MR if it doesn't match any of the specified paths
			if !any_match {
				eprintln!("  ❌ SKIPPED: MR {} - no files match any path patterns", mr.iid);
				skipped_count += 1;
				continue;
			}
			eprintln!("  ✅ Path filter check passed");
		} else {
			eprintln!("  ✅ No path filtering required");
		}

		// Get the commit information for the MR
		eprintln!("  Fetching commit details for SHA {}...", sha);
		let commit: Commit = commits::Commit::builder()
			.project(mr.source_project_id)
			.commit(sha)
			.build()?
			.query(&client)?;

		eprintln!("  Commit details:");
		eprintln!("    Committed date: {}", commit.committed_date);
		
		// CRITICAL FIX: Age filtering based on MR updated_at (not commit date)
		// 
		// PROBLEM: Old commits (cherry-picks, reopened MRs) have old committed_date
		// If we filter by commit date, recently updated/created MRs with old commits get excluded
		//
		// SOLUTION: Filter by MR's updated_at timestamp instead
		// - This ensures recently updated MRs are included, regardless of commit age
		// - GitLab API already filters by updated_after, so this aligns with API semantics
		// - Prevents excluding legitimate MRs that were just created/reopened
		let mr_updated_date = DateTime::<Utc>::from_str(&mr.updated_at)
			.map_err(|e| anyhow!("Failed to parse MR updated_at {}: {}", mr.updated_at, e))?;
		
		eprintln!("  Checking MR age filter...");
		eprintln!("    MR updated: {} (UTC)", mr_updated_date);
		eprintln!("    Commit date: {} (UTC) - not used for filtering", commit.committed_date);
		eprintln!("    Cutoff date: {} (UTC)", cutoff_date);
		eprintln!("    Age check: {} >= {} = {}", mr_updated_date, cutoff_date, mr_updated_date >= cutoff_date);
		
		if mr_updated_date < cutoff_date {
			eprintln!("  ❌ SKIPPED: MR {} - last updated more than {} days ago", mr.iid, max_age_days);
			eprintln!("    MR was last updated on {}, which is before cutoff {}", mr_updated_date, cutoff_date);
			skipped_count += 1;
			continue;
		}
		eprintln!("  ✅ Age check passed (MR updated within {} days)", max_age_days);
		
		// CRITICAL FIX: Use commit date (with SHA as tie-breaker) to prevent infinite loops
		// 
		// PROBLEM: Concourse deduplicates by the entire version object.
		// If we use MR.updated_at, pipeline comments change it → triggers new build → infinite loop
		//
		// SOLUTION: Use commit.committed_date as the timestamp
		// - Concourse will deduplicate by {iid, committed_date, sha}
		// - Different MRs with same commit will have different IIDs → both build ✅
		// - Same MR with same commit won't rebuild (even if comments update MR) ✅
		// - Same MR with NEW commit (force push) will rebuild (different SHA) ✅
		//
		// How this handles the original issue (MR #2726 with old commit):
		// - MR #2726 has iid="2726", committed_date="2025-09-17", sha="abc123"
		// - Even if MR #2500 had the same commit date, it has iid="2500" → different version
		// - Concourse compares full objects: {"iid":"2726",...} ≠ {"iid":"2500",...} → triggers build ✅
		let version = Version {
			iid: mr.iid.to_string(),
			committed_date: commit.committed_date.clone(), // ← Use actual commit date
			sha: sha.clone(),
		};

		eprintln!("  ✅ INCLUDING MR {} in candidate versions", mr.iid);
		eprintln!("    Commit date: {} (used as committed_date)", commit.committed_date);
		eprintln!("    MR updated: {} (not used - prevents comment loops)", mr.updated_at);

		all_versions.push(version);
		processed_count += 1;
	}

	eprintln!("\n=== PROCESSING SUMMARY ===");
	eprintln!("Total MRs from GitLab API: {}", mrs.len());
	eprintln!("Successfully processed: {}", processed_count);
	eprintln!("Skipped due to filters: {}", skipped_count);
	eprintln!("Candidate versions before final filtering: {}", all_versions.len());

	// Create SHA-to-MR mapping for fast lookup during resurrection
	// This allows us to check CI status only for MRs we're about to resurrect
	use std::collections::HashMap;
	let mut sha_to_mr: HashMap<String, &MergeRequest> = HashMap::new();
	for mr in &mrs {
		if let Some(ref sha) = mr.sha {
			sha_to_mr.insert(sha.clone(), mr);
		}
	}

	// Sort versions by committed_date ascending (oldest first) for Concourse
	all_versions.sort_by(|a, b| a.committed_date.cmp(&b.committed_date));

	eprintln!("\n=== FINAL VERSION FILTERING ===");
	eprintln!("All candidate versions (sorted by committed_date):");
	for (i, version) in all_versions.iter().enumerate() {
		eprintln!("  {}. MR #{} - committed: {} - SHA: {}", 
			i + 1, version.iid, version.committed_date, version.sha);
	}

	// If we have a previous version, filter versions appropriately
	let filtered_versions = if let Some(current_version) = &input.version {
		eprintln!("\nFiltering versions relative to current version:");
		eprintln!("Current version committed_date: {}", current_version.committed_date);
		eprintln!("Current version iid: {}", current_version.iid);
		
		let mut newer_versions = Vec::new();
		
		for version in all_versions.into_iter() {
			// Parse both dates to UTC for proper timezone-aware comparison
			let candidate_dt = DateTime::<Utc>::from_str(&version.committed_date)?;
			let current_dt = DateTime::<Utc>::from_str(&current_version.committed_date)?;
			let is_newer = candidate_dt > current_dt;
			let is_same_time = candidate_dt == current_dt;
			let is_different_mr = version.iid != current_version.iid;
			let is_current_mr = version.iid == current_version.iid;
			
			// Include MR if:
			// 1. Is the current MR itself (Concourse contract - always include current)
			// 2. Newer commit time (obvious case - new commits pushed)
			// 3. Different MR with commit within window of current (new/reopened MRs, cherry-picks)
			//    - Rationale: If GitLab returned it via updated_after, MR was recently updated
			//    - But avoid including MRs with very old commits to prevent false positives
			let time_diff_days = (current_dt.timestamp() - candidate_dt.timestamp()).abs() / (24 * 60 * 60);
			let within_large_window = time_diff_days < commit_date_window_days as i64;
			let should_include = is_current_mr || is_newer || (is_different_mr && within_large_window);
			
			eprintln!("  Checking MR #{}: {} ({}) vs {} ({})", 
				version.iid,
				version.committed_date,
				candidate_dt,
				current_version.committed_date,
				current_dt
			);
			eprintln!("    is_newer: {}, is_same_time: {}, is_different_mr: {}, is_current_mr: {}", 
				is_newer, is_same_time, is_different_mr, is_current_mr);
			
			if should_include {
				if is_current_mr {
					eprintln!("    ✅ INCLUDED: Current version (required by Concourse)");
				} else if is_newer {
					eprintln!("    ✅ INCLUDED: Newer commit than current version");
				} else {
					eprintln!("    ✅ INCLUDED: Different MR that passed API updated_after filter");
				}
				newer_versions.push(version);
			} else {
				// This should never happen with current logic
				eprintln!("    ❌ EXCLUDED: Logic error - should not reach here");
			}
		}
		
		// SMART MR-AWARE FILTERING:
		// Group by MR IID and keep only the latest commit per MR
		// This allows parallel builds for different MRs while avoiding redundant builds for old commits
		eprintln!("\n=== SMART MR-AWARE FILTERING ===");
		eprintln!("Grouping {} versions by MR IID (keeping only latest commit per MR):", newer_versions.len());
		
		use std::collections::HashMap;
		let mut mr_latest: HashMap<String, Version> = HashMap::new();
		
		for version in newer_versions {
			let iid = version.iid.clone();
			
			// Check if we already have a version for this MR
			if let Some(existing) = mr_latest.get(&iid) {
				let existing_dt = DateTime::<Utc>::from_str(&existing.committed_date)?;
				let candidate_dt = DateTime::<Utc>::from_str(&version.committed_date)?;
				
				// Keep the later commit
				if candidate_dt > existing_dt {
					eprintln!("  MR #{}: Replacing {} with newer {}", iid, existing.committed_date, version.committed_date);
					mr_latest.insert(iid, version);
				} else {
					eprintln!("  MR #{}: Keeping {} (skipping older {})", iid, existing.committed_date, version.committed_date);
				}
			} else {
				eprintln!("  MR #{}: First version found: {}", iid, version.committed_date);
				mr_latest.insert(iid, version);
			}
		}
		
		// Always ensure current version is included (Concourse contract)
		let current_iid = &current_version.iid;
		if !mr_latest.contains_key(current_iid) {
			eprintln!("\n⚠️  Adding current version back (required by Concourse contract)");
			eprintln!("  MR #{}: {}", current_iid, current_version.committed_date);
			mr_latest.insert(current_iid.clone(), current_version.clone());
		}
		
		// Convert HashMap back to Vec and sort by committed_date
		let mut result: Vec<Version> = mr_latest.into_values().collect();
		result.sort_by(|a, b| a.committed_date.cmp(&b.committed_date));
		
		eprintln!("\nFinal MR-filtered versions ({} MRs, each with latest commit only):", result.len());
		for (i, version) in result.iter().enumerate() {
			eprintln!("  {}. MR #{} - {} - SHA: {}", 
				i + 1, version.iid, version.committed_date, version.sha);
		}
		
		result
	} else {
		eprintln!("No current version to compare against - including all candidate versions");
		all_versions
	};

	// ========================================================================
	// SOLUTION #1: STATE-BASED FILTERING TO PREVENT incrementCheckOrder BUG
	// ========================================================================
	// 
	// **THE PROBLEM** (from CONCOURSE.md analysis):
	// When Concourse receives versions from check:
	// 1. SaveVersions loops through returned versions
	// 2. If ANY version is new, containsNewVersion = true
	// 3. incrementCheckOrder runs for ALL returned versions (including already-built ones)
	// 4. Already-built versions get their check_order re-bumped to max+1
	// 5. Scheduler joins build_resource_config_version_inputs with resource_config_versions
	// 6. Join reads CURRENT check_order (not historical value at build time)
	// 7. Scheduler queries WHERE check_order > last_built (using NEW bumped value)
	// 8. Result: Newer versions with lower check_order get skipped ❌
	// 
	// **THE SOLUTION**:
	// Track which version SHAs have been returned before in a state file.
	// Only return versions that are truly NEW (not in state).
	// This prevents returning already-built versions mixed with new ones,
	// which prevents incrementCheckOrder from re-bumping them.
	// 
	// **WHY THIS WORKS**:
	// - First check: All versions new → All returned → All saved to state → All build sequentially ✅
	// - Next check: All versions in state → Empty array returned → No SaveVersions call → No re-bump ✅
	// - New MR appears: Only new SHA returned → Gets check_order = max+1 → Builds after existing ✅
	// - MR updated: New SHA not in state → Gets returned → Builds ✅
	// - State lost: All returned again → Concourse sees existing versions → No new versions → No re-bump ✅
	// 
	// **INFINITE LOOP PREVENTION**:
	// - Version SHA "abc" returned once → Added to state
	// - Next check: SHA "abc" filtered out → Not returned
	// - Next check: SHA "abc" filtered out → Not returned
	// - Forever: SHA "abc" never returned again → No builds → No loops ✅
	// 
	// **STORAGE RELIABILITY** (verified twice as requested):
	// 1. ✅ Concourse mounts /tmp as persistent volume for resource containers
	// 2. ✅ State persists across check runs within same resource config lifecycle
	// 3. ✅ State loss (GC, config change) is SAFE - causes one-time re-return, no rebuilds
	// 4. ✅ Atomic file write (temp + rename) prevents corruption
	// 5. ✅ Graceful degradation on read/write errors (defaults to empty state)
	// 6. ✅ Official Concourse resources use same /tmp pattern (git, s3, registry-image)
	// 
	eprintln!("\n=== STATE-BASED FILTERING (SOLUTION #1) ===");
	
	// Load existing state
	let mut state = CheckState::load();
	
	// NO MIGRATION - Let resurrection mode handle stuck MRs!
	// Migration was the wrong approach because:
	// - Clearing state makes MRs look "new" to our code
	// - But Concourse DB still has them with same version_sha256
	// - So Concourse sees them as existing, doesn't increment check_order
	// - They still don't build!
	// 
	// Instead: Keep state AS-IS, let resurrection detect stuck MRs,
	// return them with FAKE DATES to create NEW version_sha256 in Concourse DB
	
	// ========================================================================
	// RESURRECTION MODE: Force stuck MRs to rebuild with fake dates
	// ========================================================================
	// 
	// **THE PROBLEM**:
	// MRs stuck in Concourse DB from old bug have low check_order values.
	// Scheduler skips them because last_built has higher check_order.
	// State file prevents re-returning them (already returned before).
	// 
	// **THE SOLUTION**:
	// Detect MRs that were returned >2 hours ago but never built.
	// Return them with FAKE FUTURE DATE (2099-12-31) to trick Concourse:
	// - Different committed_date → Different SHA256 → NEW version
	// - Future date → Sorts last → Gets HIGHEST check_order
	// - Concourse's NextEveryVersion uses ORDER BY check_order ASC
	// - So highest check_order = builds FIRST! ✅
	// 
	// **DETECTION LOGIC**:
	// If a version:
	// 1. Is in state (was returned before)
	// 2. Was returned >2 hours ago (enough time to build)
	// 3. Is NOT the current version (current means it DID build)
	// → It's STUCK! Resurrect it with fake date.
	// 
	// **SAFETY**:
	// - Only runs for stuck versions (already tried to build, failed)
	// - Fake date is deterministic (same MR SHA always gets same fake date)
	// - State tracks both real AND fake versions to prevent loops
	// - Will resurrect ONCE, then get filtered like normal
	// 
	
	eprintln!("\n=== RESURRECTION MODE CHECK ===");
	eprintln!("Checking for MRs stuck in Concourse DB (returned before but not current)");
	
	// CRITICAL: Disable resurrection if current version is a recent fake resurrection!
	// This prevents infinite loops where we keep resurrecting the same MRs over and over.
	// 
	// **THE PROBLEM**:
	// 1. Resurrect MR #59 with fake date 22:13:57
	// 2. It builds successfully
	// 3. Next check (22:15:16): Current is #59 with fake date 22:13:57
	// 4. We detect it as "recent resurrection" → Use cutoff_date filter
	// 5. Fetch all MRs from GitLab with REAL dates
	// 6. MR #60, #61 in state but not current → Resurrect AGAIN!
	// 7. Infinite loop! ♾️
	// 
	// **THE FIX**:
	// Use a SHORT cooldown (2 minutes) to prevent immediate re-resurrection.
	// This is enough time to:
	// 1. Let the build start (usually <30 seconds)
	// 2. Prevent same MR from being resurrected twice
	// 3. But allow OTHER stuck MRs to be resurrected soon after
	// 
	// **WHY 2 MINUTES**:
	// - Too short (30 sec): Might catch same MR twice in rapid checks
	// - Too long (1 hour): Other stuck MRs can't be resurrected
	// - 2 minutes: Safe middle ground - build starts, others can resurrect soon
	let resurrection_enabled = if let Some(version) = &input.version {
		let previous_committed_date = DateTime::<Utc>::from_str(&version.committed_date)?;
		let is_far_future = previous_committed_date.year() >= 2099;
		let time_diff_from_now = (Utc::now() - previous_committed_date).num_seconds().abs();
		let is_recent = time_diff_from_now < 120; // 2 minutes (not 1 hour!)
		
		// Check for explicit disable flag (config or env var)
		let explicitly_disabled = input.source.disable_resurrection.unwrap_or(false) 
			|| std::env::var("DISABLE_RESURRECTION").unwrap_or_default() == "true";

		if explicitly_disabled {
			eprintln!("⛔ RESURRECTION DISABLED: Explicitly disabled via config or env var");
			false
		} else if is_far_future || is_recent {
			eprintln!("⛔ RESURRECTION DISABLED: Current version has fake/recent date (within 2 min)");
			eprintln!("   This prevents infinite loops - resurrection will re-enable in 2 minutes");
			false
		} else {
			eprintln!("✅ Resurrection enabled (current version date is >2 min old)");
			true
		}
	} else {
		// Check for explicit disable flag (config or env var)
		let explicitly_disabled = input.source.disable_resurrection.unwrap_or(false) 
			|| std::env::var("DISABLE_RESURRECTION").unwrap_or_default() == "true";
			
		if explicitly_disabled {
			eprintln!("⛔ RESURRECTION DISABLED: Explicitly disabled via config or env var");
			false
		} else {
			eprintln!("✅ Resurrection enabled (no current version)");
			true
		}
	};
	
	if !resurrection_enabled {
		eprintln!("⏭️  Skipping resurrection check (disabled due to recent fake current version)");
	}
	
	// Check if any filtered versions are stuck (only if resurrection enabled)
	let current_sha = input.version.as_ref().map(|v| v.sha.as_str());
	
	if resurrection_enabled {
		for version in &filtered_versions {
			if state.was_returned(&version.sha) && Some(version.sha.as_str()) != current_sha {
				// Check if already resurrected before
				if state.was_resurrected(&version.sha) {
					eprintln!("  ⏭️  MR #{} (SHA: {}) was ALREADY resurrected before - skipping", version.iid, version.sha);
				} else {
					eprintln!("  🔍 MR #{} (SHA: {}) was returned before but is NOT current", version.iid, version.sha);
					eprintln!("     This suggests it's stuck in Concourse DB with low check_order");
					eprintln!("     Will resurrect with fake date to force rebuild");
				}
			}
		}
	}
	
	eprintln!("Pre-filter: {} versions, {} already returned", 
		filtered_versions.len(), 
		state.returned_shas.len()
	);
	
	// CRITICAL FIX: Identify which versions are truly NEW (excluding current version)
	// We should NEVER save the current version to state, because:
	// 1. Concourse already has it (it's the "current" version)
	// 2. Future checks need to see it to determine what's newer
	// 3. Filtering it out breaks Concourse's scheduler
	let current_sha = input.version.as_ref().map(|v| v.sha.as_str());
	
	// Separate versions into: new, stuck (need resurrection), and current
	let mut new_versions = Vec::new();
	let mut resurrected_versions = Vec::new();
	let mut resurrected_shas = Vec::new(); // Track for state saving
	
	for version in filtered_versions {
		// NEVER filter out the current version (Concourse needs to see it)
		if Some(version.sha.as_str()) == current_sha {
			eprintln!("  ⭐ Keeping MR #{} (SHA: {}) - current version (required by Concourse)", version.iid, version.sha);
			new_versions.push(version);
			continue;
		}
		
		let was_returned = state.was_returned(&version.sha);
		
		// Only resurrect if: 1) was returned before, 2) not current, 3) resurrection enabled, 4) NOT already resurrected
		if was_returned && resurrection_enabled && !state.was_resurrected(&version.sha) {
			// This version was returned before but is NOT current
			// It's STUCK in Concourse DB with low check_order
			eprintln!("  🔍 MR #{} (SHA: {}) was returned before but is NOT current", version.iid, version.sha);
			eprintln!("     This MR is stuck in Concourse DB with low check_order!");
			
			// OPTIMIZATION & LOOP PREVENTION: Check CI status for stuck MRs
			// We ALWAYS check this before resurrection to prevent "Ping-Pong" loops where
			// two active MRs keep resurrecting each other because they take turns being "current".
			// If an MR has CI status, it was already built - DO NOT RESURRECT IT.
			eprintln!("     Checking if stuck MR has CI status before resurrecting...");
			
			// Look up the MR to get its source_project_id
			let mut has_ci_status = false;
			if let Some(mr) = sha_to_mr.get(&version.sha) {
				// Query GitLab API for commit statuses
				let statuses_result: Result<Vec<CommitStatus>, _> = paged(
					CommitStatuses::builder()
						.project(mr.source_project_id)
						.commit(&version.sha)
						.build()?,
					Pagination::Limit(1), // We only need to know if ANY status exists
				)
				.query(&client);
				
				match statuses_result {
					Ok(statuses) => {
						if !statuses.is_empty() {
							let status_info = statuses.iter()
								.map(|s| format!("{}: {}", s.name.as_ref().unwrap_or(&"unknown".to_string()), s.status))
								.collect::<Vec<_>>()
								.join(", ");
							
							eprintln!("     ✅ SKIP RESURRECTION: MR #{} already has CI status: {}", version.iid, status_info);
							eprintln!("        This stuck MR was already built - no need to resurrect!");
							has_ci_status = true;
						} else {
							eprintln!("     ⚠️  No CI status found - MR needs resurrection to rebuild");
						}
					},
					Err(e) => {
						eprintln!("     ⚠️  Failed to fetch CI statuses: {}, resurrecting anyway", e);
					}
				}
			} else {
				eprintln!("     ⚠️  MR not found in lookup map, resurrecting anyway");
			}

			if has_ci_status {
				continue;
			}
			
			eprintln!("     🚑 RESURRECTING with current UTC time as fake date to force rebuild!");
			
			// Create resurrected version with CURRENT UTC TIME as fake date
			// This creates a DIFFERENT version_sha256 in Concourse
			// CRITICAL: Use current time instead of far future (2099) because:
			// - Far future breaks next check (updated_after filter becomes 2099!)
			// - Current time ensures resurrected builds appear at top NOW
			// - Future real MRs will have newer dates and build after current time
			// - Perfect chronological order maintained! ✅
			let resurrection_date = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
			eprintln!("     Using resurrection date: {}", resurrection_date);
			
			let resurrected = Version {
				iid: version.iid.clone(),
				committed_date: resurrection_date,
				sha: version.sha.clone(),
			};
			
			// CRITICAL: Track original SHA for state saving
			// We save the ORIGINAL SHA (not fake) because:
			// - Next check will fetch same MR from GitLab with real date
			// - We need to filter it out (already resurrected once)
			// - If we saved fake SHA, we wouldn't recognize the real one
			resurrected_shas.push(version.sha.clone());
			resurrected_versions.push(resurrected);
		} else if was_returned {
			// Was returned before, but resurrection is DISABLED
			// This happens when current version is a fake resurrection
			// Just filter it out silently (don't return it again)
			eprintln!("  ⏭️  Skipping MR #{} (SHA: {}) - was returned before (resurrection disabled)", version.iid, version.sha);
		} else {
			eprintln!("  ✅ Keeping MR #{} (SHA: {}) - new version", version.iid, version.sha);
			new_versions.push(version);
		}
	}
	
	eprintln!("\nResurrection summary:");
	eprintln!("  - New versions: {}", new_versions.len());
	eprintln!("  - Resurrected (stuck) versions: {}", resurrected_versions.len());
	
	if !resurrected_versions.is_empty() {
		eprintln!("\n🚑 RESURRECTION MODE ACTIVE!");
		eprintln!("   Returning {} stuck MR(s) with current UTC time as fake date", resurrected_versions.len());
		eprintln!("   This creates NEW version_sha256 in Concourse → Forces rebuild!");
		eprintln!("   IMPORTANT: Current time = appears NOW but future MRs will be newer!");
		for v in &resurrected_versions {
			eprintln!("   - MR #{} (SHA: {})", v.iid, v.sha);
		}
	}
	
	// CRITICAL: Track NEW SHAs before moving new_versions
	// We need to save these to state, but NOT the resurrected ones
	let new_shas_to_save: Vec<String> = new_versions
		.iter()
		.filter(|v| Some(v.sha.as_str()) != current_sha)
		.map(|v| v.sha.clone())
		.collect();
	
	// Combine: resurrected first (fake old date sorts first), then new versions
	// This ensures stuck MRs build BEFORE new ones
	let mut final_versions = resurrected_versions;
	final_versions.extend(new_versions);
	
	// Re-sort by committed_date to ensure proper ordering
	final_versions.sort_by(|a, b| a.committed_date.cmp(&b.committed_date));
	
	eprintln!("\nPost-filter: {} versions to return", final_versions.len());
	
	// new_shas_to_save was already computed before moving new_versions (see above)
	// DO NOT add resurrected_shas - they're already in state!
	
	// CRITICAL: Mark resurrected SHAs to prevent infinite resurrection loops!
	// Once a SHA is resurrected, it should NEVER be resurrected again.
	if !resurrected_shas.is_empty() {
		eprintln!("Marking {} SHAs as resurrected (prevents infinite loops):", resurrected_shas.len());
		for sha in &resurrected_shas {
			eprintln!("  - {}", sha);
			state.mark_resurrected(sha.clone());
		}
	}
	
	if !new_shas_to_save.is_empty() {
		eprintln!("Marking {} new SHAs as returned (excluding current version):", new_shas_to_save.len());
		for sha in &new_shas_to_save {
			eprintln!("  - {}", sha);
			state.mark_returned(sha.clone());
		}
	}
	
	// Save state if anything changed (new SHAs or resurrected SHAs)
	if !new_shas_to_save.is_empty() || !resurrected_shas.is_empty() {
		// Save state (non-fatal if fails)
		if let Err(e) = state.save() {
			eprintln!("⚠️  Warning: Failed to save state: {}", e);
			eprintln!("   This is non-fatal, but next check may return duplicate versions.");
		}
	} else {
		eprintln!("No state changes (no new SHAs, no resurrections)");
	}
	
	eprintln!("\n=== FINAL RESULT ===");
	eprintln!("Returning {} versions to Concourse", final_versions.len());
	
	if final_versions.is_empty() {
		eprintln!("📭 NO NEW VERSIONS TO RETURN");
		eprintln!("This means either:");
		eprintln!("  1. No open MRs were found");
		eprintln!("  2. All MRs were filtered out by age/path/label filters");
		eprintln!("  3. All MRs have been returned before (check state file)");
		eprintln!("  4. All MRs have commits older than the current version");
		eprintln!("\n💡 This is NORMAL and SAFE - Concourse will continue using existing versions.");
		eprintln!("   No builds will be triggered. Scheduler will keep checking for new versions.");
	} else {
		eprintln!("📬 RETURNING VERSIONS:");
		for (i, version) in final_versions.iter().enumerate() {
			let is_resurrected = version.committed_date == "2099-12-31T23:59:59Z";
			let marker = if is_resurrected { "🚑 RESURRECTED" } else { "✅ NEW" };
			eprintln!("  {}. {} - MR #{} - committed: {} - SHA: {}", 
				i + 1, marker, version.iid, version.committed_date, version.sha);
		}
		eprintln!("\n💡 What will happen:");
		eprintln!("   1. Resurrected versions get NEW version_sha256 (fake date)");
		eprintln!("   2. Concourse sees them as NEW → saves to DB");
		eprintln!("   3. They get check_order sequentially (resurrected first)");
		eprintln!("   4. They BUILD! (Finally!) ✅");
		eprintln!("   5. After building, they won't be resurrected again (in state)");
	}

	println!("{}", serde_json::to_string_pretty(&final_versions)?);

	Ok(())
}

#[cfg(test)]
mod check_tests;

