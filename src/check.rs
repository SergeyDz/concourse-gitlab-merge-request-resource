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
	Pagination,
	Query,
};
use gitlab::Gitlab;
use glob::Pattern;
use serde::Deserialize;
use std::io;
use std::str::FromStr;
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
	let client = Gitlab::new(uri.host_str().unwrap(), &input.source.private_token)?;

	// Calculate the cutoff date for maximum age (default: 90 days / 3 months)
	let max_age_days = input.source.max_age_days.unwrap_or(90);
	let cutoff_date = Utc::now() - chrono::Duration::days(max_age_days as i64);

	eprintln!("=== CONCOURSE GITLAB MR RESOURCE DEBUG INFO ===");
	eprintln!("Current time (UTC): {}", Utc::now());
	eprintln!("Max age days: {}", max_age_days);
	eprintln!("Cutoff date: {}", cutoff_date);

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
		let margin = chrono::Duration::minutes(10);
		let filter_time = previous_committed_date - margin;
		eprintln!("Using previous version's committed_date - 10min margin as updated_after filter: {}", filter_time);
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
		// Parse the MR updated date to check if it's within our age limit
		let mr_updated_at = DateTime::<Utc>::from_str(&mr.updated_at)
			.map_err(|e| anyhow!("Failed to parse MR updated_at {}: {}", mr.updated_at, e))?;

		eprintln!("  Checking age filter...");
		eprintln!("    MR updated at: {} (UTC)", mr_updated_at);
		eprintln!("    Cutoff date: {} (UTC)", cutoff_date);
		eprintln!("    Age check: {} > {} = {}", mr_updated_at, cutoff_date, mr_updated_at >= cutoff_date);

		// Skip MRs that are too old
		if mr_updated_at < cutoff_date {
			eprintln!("  ❌ SKIPPED: MR {} is older than {} days", mr.iid, max_age_days);
			skipped_count += 1;
			continue;
		}
		eprintln!("  ✅ Age check passed");

		// Apply path filtering if specified
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
		
		let version = Version {
			iid: mr.iid.to_string(),
			committed_date: commit.committed_date.clone(),
			sha: mr.sha.clone(),
		};

		eprintln!("  ✅ INCLUDING MR {} in candidate versions", mr.iid);
		eprintln!("    MR updated: {}", mr.updated_at);
		eprintln!("    Commit date: {}", commit.committed_date);

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

	// If we have a previous version, only return versions newer than it
	let filtered_versions = if let Some(current_version) = &input.version {
		eprintln!("\nFiltering versions newer than current version:");
		eprintln!("Current version committed_date: {}", current_version.committed_date);
		eprintln!("Current version iid: {}", current_version.iid);
		
		let mut newer_versions = Vec::new();
		let mut found_bulk_mrs = false;
		
		for version in all_versions.into_iter() {
			// Parse both dates to UTC for proper timezone-aware comparison
			let candidate_dt = DateTime::<Utc>::from_str(&version.committed_date)?;
			let current_dt = DateTime::<Utc>::from_str(&current_version.committed_date)?;
			let is_newer = candidate_dt > current_dt;
			let is_same_time = candidate_dt == current_dt;
			let is_different_mr = version.iid != current_version.iid;
			let is_current_mr = version.iid == current_version.iid;
			
			// Calculate time window for bulk MR detection
			let time_diff_minutes = (current_dt.timestamp() - candidate_dt.timestamp()) / 60;
			let within_bulk_window = (0..=10).contains(&time_diff_minutes);
			
			// Include MR if:
			// 1. Newer commit time (obvious case)
			// 2. Same commit time AND different MR (crossplane case - same SHA)
			// 3. Within 10-minute window AND different MR (external-dns case - bulk creation)
			// 4. Is the current MR itself (Concourse expects this to be included)
			let is_newer_or_different_mr = is_newer 
				|| (is_same_time && is_different_mr)
				|| (within_bulk_window && is_different_mr)
				|| is_current_mr;
			
			// Track if we found bulk MRs (older MRs within window)
			if within_bulk_window && is_different_mr && !is_newer && !is_same_time {
				found_bulk_mrs = true;
			}
			
			eprintln!("  Checking MR #{}: {} ({}) vs {} ({})", 
				version.iid,
				version.committed_date,
				candidate_dt,
				current_version.committed_date,
				current_dt
			);
			eprintln!("    is_newer: {}, is_same_time: {}, is_different_mr: {}, is_current_mr: {}", 
				is_newer, is_same_time, is_different_mr, is_current_mr);
			eprintln!("    time_diff_minutes: {}, within_bulk_window: {}", 
				time_diff_minutes, within_bulk_window);
			
			if is_newer_or_different_mr {
				if is_current_mr {
					eprintln!("    ✅ INCLUDED: Current version (required by Concourse)");
				} else if is_newer {
					eprintln!("    ✅ INCLUDED: Newer than current version");
				} else if is_same_time {
					eprintln!("    ✅ INCLUDED: Same commit time but different MR (same SHA scenario)");
				} else {
					eprintln!("    ✅ INCLUDED: Within bulk creation window (created within 10 minutes)");
				}
				newer_versions.push(version);
			} else {
				eprintln!("    ❌ EXCLUDED: Not newer and not within bulk window, and not current MR");
			}
		}
		
		// If we found bulk MRs, log a warning about the reprocessing
		if found_bulk_mrs {
			eprintln!("\n⚠️  BULK MR DETECTION: Found MRs created within 10-minute window");
			eprintln!("    This indicates bulk MR creation that was processed out of order.");
			eprintln!("    Returning all MRs in chronological order for Concourse to reprocess.");
			eprintln!("    This is similar to a 'force push' scenario in git resources.");
		}
		
		newer_versions
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
