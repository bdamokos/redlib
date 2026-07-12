use std::fs;
use std::path::{Path, PathBuf};
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
	println!("cargo:rustc-env=STATIC_ASSET_REVISION={}", static_asset_revision(Path::new("static")));
}

fn static_asset_revision(root: &Path) -> String {
	fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) {
		let Ok(entries) = fs::read_dir(dir) else {
			return;
		};
		for entry in entries.flatten() {
			let path = entry.path();
			if path.is_dir() {
				collect_files(&path, files);
			} else if path.is_file() {
				files.push(path);
			}
		}
	}

	fn update(hash: &mut u64, bytes: &[u8], prime: u64) {
		for byte in bytes {
			*hash ^= u64::from(*byte);
			*hash = hash.wrapping_mul(prime);
		}
	}

	let mut files = Vec::new();
	collect_files(root, &mut files);
	files.sort();
	let mut first = 0xcbf29ce484222325_u64;
	let mut second = 0x84222325cbf29ce4_u64;
	for path in files {
		let relative = path.strip_prefix(root).unwrap_or(&path).to_string_lossy();
		update(&mut first, relative.as_bytes(), 0x100000001b3);
		update(&mut second, relative.as_bytes(), 0x100000001b7);
		if let Ok(bytes) = fs::read(&path) {
			update(&mut first, &bytes, 0x100000001b3);
			update(&mut second, &bytes, 0x100000001b7);
		}
	}
	format!("asset-{first:016x}{second:016x}")
}
