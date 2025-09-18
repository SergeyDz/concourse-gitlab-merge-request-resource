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

	// Determine the starting point for filtering
	let updated_after = if let Some(version) = &input.version {
		// If we have a previous version, use its committed date as the starting point
		DateTime::<Utc>::from_str(&version.committed_date)?
	} else {
		// For initial run, use the cutoff date
		cutoff_date
	};

	let project_path = uri.path().trim_start_matches('/').trim_end_matches(".git");

	// Build the query for opened merge requests only
	let mut builder = MergeRequests::builder();
	builder
		.project(project_path)
		.state(MergeRequestState::Opened) // ONLY fetch opened MRs - this fixes the core issue!
		.order_by(MergeRequestOrderBy::UpdatedAt)
		.sort(SortOrder::Descending) // Most recent first for efficiency
		.updated_after(updated_after);

	// Apply optional filters
	if let Some(target_branch) = &input.source.target_branch {
		builder.target_branch(target_branch);
	}

	if let Some(labels) = &input.source.labels {
		builder.labels(labels.iter());
	}

	if let Some(skip_draft) = input.source.skip_draft {
		if skip_draft {
			builder.wip(YesNo::No);
		}
	}

	// Use pagination to get all results (GitLab limits to 100 per page by default)
	let mrs: Vec<MergeRequest> = paged(builder.build()?, Pagination::All)
		.query(&client)?;

	eprintln!("Found {} opened merge requests", mrs.len());

	let mut all_versions = Vec::<Version>::new();

	// Process each merge request
	for mr in mrs {
		// Parse the MR updated date to check if it's within our age limit
		let mr_updated_at = DateTime::<Utc>::from_str(&mr.updated_at)
			.map_err(|e| anyhow!("Failed to parse MR updated_at {}: {}", mr.updated_at, e))?;

		// Skip MRs that are too old
		if mr_updated_at < cutoff_date {
			eprintln!("Skipping MR {} (updated: {}) - older than {} days", mr.iid, mr.updated_at, max_age_days);
			continue;
		}

		// Apply path filtering if specified
		if let Some(paths) = &input.source.paths {
			let patterns: Vec<Pattern> = paths.iter().map(|path| Pattern::new(path).unwrap()).collect();
			let diffs: Vec<Diff> = MergeRequestDiffs::builder()
				.project(project_path)
				.merge_request(mr.iid)
				.build()?
				.query(&client)?;
			
			// Skip MR if it doesn't match any of the specified paths
			if !diffs
				.iter()
				.any(|diff| patterns.iter().any(|pattern| pattern.matches(&diff.new_path)))
			{
				eprintln!("Skipping MR {} - no matching paths", mr.iid);
				continue;
			}
		}

		// Get the commit information for the MR
		let commit: Commit = commits::Commit::builder()
			.project(mr.source_project_id)
			.commit(&mr.sha)
			.build()?
			.query(&client)?;

		eprintln!("Including MR {} (updated: {}, committed: {})", mr.iid, mr.updated_at, commit.committed_date);

		all_versions.push(Version {
			iid: mr.iid.to_string(),
			committed_date: commit.committed_date,
			sha: mr.sha,
		});
	}

	// Sort versions by committed_date ascending (oldest first) for Concourse
	all_versions.sort_by(|a, b| a.committed_date.cmp(&b.committed_date));

	// If we have a previous version, only return versions newer than it
	let filtered_versions = if let Some(current_version) = &input.version {
		all_versions
			.into_iter()
			.filter(|v| v.committed_date > current_version.committed_date)
			.collect()
	} else {
		all_versions
	};

	eprintln!("Returning {} versions", filtered_versions.len());
	println!("{}", serde_json::to_string_pretty(&filtered_versions)?);

	Ok(())
}