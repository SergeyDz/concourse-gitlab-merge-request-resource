mod common;
use common::*;
use std::io;
use std::fs::File;
use serde::{Serialize, Deserialize};
use serde_json;
use clap::Parser;
use std::path::Path;
use url::Url;
use gitlab::{ Gitlab, api::{ projects::{ repository::commits, merge_requests }, Query} };
use std::env;
use anyhow::{ Result, anyhow, Context };

#[derive(Debug, Deserialize)]
struct Params {
	resource_name: String,
	status: String,
	pipeline_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResourceInput {
	source: Source,
	params: Params,
}

#[derive(Debug, Serialize)]
struct ResourceOutput {
	version: Version,
	metadata: Vec<Metadata>,
}

#[derive(Parser)]
struct Args {
	#[arg()]
	directory: String,
}

fn main() -> Result<()> {
	let args = Args::parse();

	let input: ResourceInput = get_data_from(&mut io::stdin()).map_err(|err| anyhow!("{}", err.downcast::<serde_json::Error>().unwrap()))?;
	let version: Version = serde_json::from_reader(
		File::open(Path::new(&args.directory).join(&input.params.resource_name).join(".merge-request.json"))?
	).with_context(|| anyhow!("failed to read `.merge-request.json`"))?;

	let uri = Url::parse(&input.source.uri)?;
	let client = Gitlab::new(
		uri.host_str().unwrap(),
		&input.source.private_token,
	)?;

	let mr: MergeRequest = merge_requests::MergeRequest::builder()
		.project(uri.path().trim_start_matches("/").trim_end_matches(".git"))
		.merge_request(version.iid.parse::<u64>().unwrap())
		.build()?
		.query(&client)?;

	/* get environment variables */
	let build_pipeline_name = env::var("BUILD_PIPELINE_NAME").with_context(|| anyhow!("BUILD_PIPELINE_NAME is not set"))?;
	let build_job_name = env::var("BUILD_JOB_NAME").with_context(|| anyhow!("BUILD_JOB_NAME is not set"))?;
	let build_team_name = env::var("BUILD_TEAM_NAME").with_context(|| anyhow!("BUILD_TEAM_NAME is not set"))?;
	let build_name = env::var("BUILD_NAME").with_context(|| anyhow!("BUILD_NAME is not set"))?;
	let build_pipeline_instance_vars = match env::var("BUILD_PIPELINE_INSTANCE_VARS") {
		Ok(v) => {
			let instance_vars: serde_json::Value = serde_json::from_str(&v).unwrap();
			let mut params = String::from("?");
			for (key, value) in instance_vars.as_object().unwrap().iter() {
				params.push_str(&format!("{}={}&", key, value.as_str().unwrap()));
			}
			params.pop();
			params
		},
		Err(_) => "".to_owned(),
	};

	let concourse_uri = format!(
		"{}/teams/{}/pipelines/{}/jobs/{}/builds/{}{}",
		env::var("ATC_EXTERNAL_URL").with_context(|| anyhow!("ATC_EXTERNAL_URL is not set"))?,
		&build_team_name,
		&build_pipeline_name,
		&build_job_name,
		&build_name,
		&build_pipeline_instance_vars,
	);

	let pipeline_name = if let Some(pipeline_name) = &input.params.pipeline_name {
		pipeline_name.clone()
			.replace("%BUILD_PIPELINE_NAME%", &build_pipeline_name)
			.replace("%BUILD_JOB_NAME%", &build_job_name)
			.replace("%BUILD_TEAM_NAME%", &build_team_name)
			.replace("%BUILD_PIPELINE_INSTANCE_VARS%", &build_pipeline_instance_vars)
	} else {
		format!("{}::{}", build_team_name, build_pipeline_name)
	};

	let responce: CommitStatusResponce = commits::CreateCommitStatus::builder()
		.project(mr.source_project_id)
		.commit(&version.sha)
		.state(
			match input.params.status.as_str() {
				"canceled" => commits::CommitStatusState::Canceled,
				"running" => commits::CommitStatusState::Running,
				"pending" => commits::CommitStatusState::Pending,
				"failed" => commits::CommitStatusState::Failed,
				"success" => commits::CommitStatusState::Success,
				_ => panic!("invalid status")
			}
		)
		.name(&pipeline_name)
		.target_url(&concourse_uri)
		.build()?
		.query(&client)?;

	let output = ResourceOutput {
		version: version,
		metadata: vec![
			Metadata { name: "url".to_owned(), value: mr.web_url },
			Metadata { name: "author".to_owned(), value: mr.author.name },
			Metadata { name: "title".to_owned(), value: mr.title },
			Metadata { name: "status".to_owned(), value: responce.status },
		]
	};
	println!("{}", serde_json::to_string_pretty(&output)?);
	Ok(())
}
