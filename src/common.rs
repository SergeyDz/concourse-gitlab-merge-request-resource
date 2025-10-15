use serde::{
	Deserialize,
	Serialize,
};
use std::error;
use std::io;

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[allow(dead_code)]
pub struct Params {
	pub status: Option<String>,
	pub coverage: Option<f32>,
}

#[derive(Debug, Deserialize, Serialize)]
#[allow(dead_code)]
pub struct Metadata {
	pub name: String,
	pub value: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct CommitStatusResponce {
	pub status: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct CommitStatus {
	pub id: u64,
	pub sha: String,
	pub status: String,
	pub name: Option<String>,
	pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Commit {
	pub committed_date: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Project {
	pub http_url_to_repo: String,
	pub ssh_url_to_repo: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Author {
	pub name: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Change {
	pub new_path: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct MergeRequestChanges {
	pub changes: Vec<Change>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Diff {
	pub old_path: String,
	pub new_path: String,
	pub a_mode: String,
	pub b_mode: String,
	pub diff: String,
	pub new_file: bool,
	pub renamed_file: bool,
	pub deleted_file: bool,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct MergeRequest {
	pub iid: u64,
	pub title: String,
	pub labels: Vec<String>,
	pub sha: String,
	pub author: Author,
	pub updated_at: String,
	pub source_project_id: u64,
	pub source_branch: String,
	pub web_url: String,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Clone)]
pub struct Version {
	pub iid: String,
	pub committed_date: String,
	pub sha: String,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Source {
	pub uri: String,
	pub private_token: String,
	pub labels: Option<Vec<String>>,
	pub paths: Option<Vec<String>>,
	pub skip_draft: Option<bool>,
	pub target_branch: Option<String>,
	/// Maximum age in days for merge requests to be considered (default: 90 days / 3 months)
	pub max_age_days: Option<u32>,
	/// Skip MRs where the last commit has any CI status (prevents rebuilding already-built MRs)
	pub skip_mr_with_ci_status: Option<bool>,
}

pub fn get_data_from<T: for<'de> Deserialize<'de>>(stdin: &mut impl io::Read) -> Result<T, Box<dyn error::Error>> {
	let mut buffer = String::new();
	stdin.read_to_string(&mut buffer)?;
	Ok(serde_json::from_str(&buffer)?)
}

#[cfg(test)]
mod tests {
	use super::{
		get_data_from,
		Deserialize,
		Source,
		Version,
	};

	#[test]
	fn test_get_data_from() {
		#[derive(Debug, Deserialize, PartialEq)]
		struct ResourceInput {
			source: Source,
			version: Option<Version>,
		}

		let dummy = r#"
			{
				"source": {
					"uri": "https://gitlab.com/cheatsc/test.git",
					"private_token": "zzzzz"
				}
			}
		"#;
		assert_eq!(
			get_data_from::<ResourceInput>(&mut dummy.as_bytes()).unwrap(),
			ResourceInput {
				source: Source {
					uri: "https://gitlab.com/cheatsc/test.git".to_owned(),
					private_token: "zzzzz".to_owned(),
					labels: None,
					paths: None,
					skip_draft: None,
					target_branch: None,
					max_age_days: None,
					skip_mr_with_ci_status: None,
				},
				version: None,
			}
		);
	}
}
