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

	let mut builder = MergeRequests::builder();
	builder
		.project(uri.path().trim_start_matches('/')
		.trim_end_matches(".git"))
		.order_by(MergeRequestOrderBy::UpdatedAt)
		.sort(SortOrder::Ascending);

	/* Apply state filter only if we don't have a previous version */
	if input.version.is_none() {
		builder.state(MergeRequestState::Opened);
	} else {
		/* When we have a previous version, look at all states so we don't miss new MRs */
		builder.state(MergeRequestState::All);
	}

	/* filter mrs by target branch */
	if let Some(target_branch) = &input.source.target_branch {
		builder.target_branch(target_branch);  // This line filters MRs by target branch
	}

	/* filter mrs by updated date */
	if let Some(version) = &input.version {
		builder.updated_after(DateTime::<Utc>::from_str(&version.committed_date)?);
	}

	/* filter mrs by labels */
	if let Some(labels) = &input.source.labels {
		builder.labels(labels.iter());
	}

	/* filter mrs by draft */
	if let Some(skip_draft) = input.source.skip_draft {
		if skip_draft {
			builder.wip(YesNo::No);
		}
	}

	let mrs: Vec<MergeRequest> = builder.build()?.query(&client)?;

	let mut versions = Vec::<Version>::new();
	for mr in mrs {
		/* filter mrs by filepath in their changes */
		if let Some(paths) = &input.source.paths {
			let patterns: Vec<Pattern> = paths.iter().map(|path| Pattern::new(path).unwrap()).collect();
			let diffs: Vec<Diff> = MergeRequestDiffs::builder()
				.project(uri.path().trim_start_matches('/').trim_end_matches(".git"))
				.merge_request(mr.iid)
				.build()?
				.query(&client)?;
			if !diffs
				.iter()
				.any(|diff| patterns.iter().any(|pattern| pattern.matches(&diff.new_path)))
			{
				continue;
			}
		}

		let commit: Commit = commits::Commit::builder()
			.project(mr.source_project_id)
			.commit(&mr.sha)
			.build()?
			.query(&client)?;
		versions.push(Version {
			iid: mr.iid.to_string(),
			committed_date: commit.committed_date,
			sha: mr.sha,
		});
	}

	println!("{}", serde_json::to_string_pretty(&versions)?);

	Ok(())
}
