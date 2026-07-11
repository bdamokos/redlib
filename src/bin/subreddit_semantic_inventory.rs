use serde::Deserialize;
use std::{env, fs};

#[derive(Deserialize)]
struct Entry {
	subreddit: String,
	#[allow(dead_code)]
	model: String,
	#[allow(dead_code)]
	text_hash: String,
	#[allow(dead_code)]
	embedding: Vec<f32>,
	#[allow(dead_code)]
	embedded_at_unix: u64,
}

#[derive(Deserialize)]
struct File {
	#[allow(dead_code)]
	version: u8,
	#[allow(dead_code)]
	saved_at_unix: u64,
	entries: Vec<Entry>,
}

fn main() -> Result<(), String> {
	let path = env::args().nth(1).ok_or_else(|| "usage: subreddit_semantic_inventory SIDECAR".to_string())?;
	let bytes = fs::read(&path).map_err(|error| format!("read {path}: {error}"))?;
	let file: File = bincode::deserialize(&bytes).map_err(|error| format!("decode {path}: {error}"))?;
	let mut names = file.entries.into_iter().map(|entry| entry.subreddit).collect::<Vec<_>>();
	names.sort_unstable();
	names.dedup();
	for name in names {
		println!("{name}");
	}
	Ok(())
}
