mod common;
use anyhow::{
	anyhow,
	Result,
};
use chrono::{
	DateTime,
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
		repository::commits,
	},
	paged,
	retry::{Backoff, Client as RetryClient},
	Pagination,
	Query,
};
use gitlab::Gitlab;
use glob::Pattern;
use serde::Deserialize;
use std::io;
use std::str::FromStr;
use std::time::Duration;
use url::Url;

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

	eprintln!("=== CONCOURSE GITLAB MR RESOURCE DEBUG INFO ===");
	eprintln!("Current time (UTC): {}", Utc::now());
	eprintln!("Max age days: {}", max_age_days);
	eprintln!("Cutoff date: {}", cutoff_date);
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
		
		// Subtract margin to catch bulk-created MRs
		// This handles cases where multiple MRs are created/updated within a short time window
		// IMPORTANT: Margin must be SMALL to prevent infinite loops from pipeline comments
		// If margin >= build time, comments will retrigger builds infinitely
		let margin = chrono::Duration::minutes(10);
		let filter_time = previous_committed_date - margin;
		eprintln!("Using previous version's committed_date - {}min margin as updated_after filter: {}", margin.num_minutes(), filter_time);
		filter_time
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
		eprintln!("  SHA: {}", mr.sha);
		eprintln!("  Source branch: {}", mr.source_branch);
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
		eprintln!("  Fetching commit details for SHA {}...", mr.sha);
		let commit: Commit = commits::Commit::builder()
			.project(mr.source_project_id)
			.commit(&mr.sha)
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
			sha: mr.sha.clone(),
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
			// 3. Different MR with commit within 30 days of current (new/reopened MRs, cherry-picks)
			//    - Rationale: If GitLab returned it via updated_after, MR was recently updated
			//    - But avoid including MRs with very old commits (>30 days) to prevent false positives
			let time_diff_days = (current_dt.timestamp() - candidate_dt.timestamp()).abs() / (24 * 60 * 60);
			let within_large_window = time_diff_days < 90;  // 90 days window (same as age cutoff)
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

	eprintln!("\n=== FINAL RESULT ===");
	eprintln!("Returning {} versions to Concourse", filtered_versions.len());
	
	if filtered_versions.is_empty() {
		eprintln!("⚠️  NO VERSIONS TO RETURN!");
		eprintln!("This means either:");
		eprintln!("  1. No open MRs were found");
		eprintln!("  2. All MRs were filtered out by age/path/label filters");
		eprintln!("  3. All MRs have commits older than the current version");
		eprintln!("Check the logs above to see which case applies.");
	} else {
		eprintln!("Final versions being returned:");
		for (i, version) in filtered_versions.iter().enumerate() {
			eprintln!("  {}. MR #{} - committed: {} - SHA: {}", 
				i + 1, version.iid, version.committed_date, version.sha);
		}
	}

	println!("{}", serde_json::to_string_pretty(&filtered_versions)?);

	Ok(())
}

#[cfg(test)]
mod check_tests;

