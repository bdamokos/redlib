use std::process::{Command, ExitStatus, Output};

#[cfg(not(target_os = "windows"))]
use std::os::unix::process::ExitStatusExt;

#[cfg(target_os = "windows")]
use std::os::windows::process::ExitStatusExt;

fn main() {
	println!("cargo:rerun-if-env-changed=GIT_HASH");
	println!("cargo:rerun-if-changed=.git/HEAD");
	println!("cargo:rerun-if-changed=src/");
	println!("cargo:rerun-if-changed=static/");
	println!("cargo:rerun-if-changed=templates/");
	let output = String::from_utf8(
		Command::new("git")
			.args(["rev-parse", "HEAD"])
			.output()
			.unwrap_or(Output {
				stdout: vec![],
				stderr: vec![],
				status: ExitStatus::from_raw(0),
			})
			.stdout,
	)
	.unwrap_or_default();
	let git_hash = if output == String::default() {
		std::env::var("GIT_HASH").unwrap_or_else(|_| "dev".into())
	} else {
		output
	};
	println!("cargo:rustc-env=GIT_HASH={git_hash}");
}
