use std::fs;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Output};

#[cfg(not(target_os = "windows"))]
use std::os::unix::process::ExitStatusExt;

#[cfg(target_os = "windows")]
use std::os::windows::process::ExitStatusExt;

fn main() {
	println!("cargo:rerun-if-changed=src/");
	println!("cargo:rerun-if-changed=static/style.css");
	println!("cargo:rerun-if-changed=static/themes/");
	println!("cargo:rerun-if-changed=templates/base.html");
	println!("cargo:rerun-if-env-changed=REDLIB_BUILD_GIT_SHA");
	println!("cargo:rerun-if-env-changed=REDLIB_BUILD_UPSTREAM_AHEAD");
	println!("cargo:rerun-if-env-changed=REDLIB_BUILD_UPSTREAM_BEHIND");

	let output = Command::new("git").args(["rev-parse", "HEAD"]).output().unwrap_or(Output {
		stdout: vec![],
		stderr: vec![],
		status: ExitStatus::from_raw(1),
	});
	let discovered_git_hash = if output.status.success() {
		String::from_utf8(output.stdout).unwrap_or_default().trim().to_string()
	} else {
		String::new()
	};
	let git_hash = std::env::var("REDLIB_BUILD_GIT_SHA")
		.ok()
		.filter(|value| !value.trim().is_empty())
		.unwrap_or(discovered_git_hash);
	let git_hash = if git_hash.is_empty() { "dev" } else { &git_hash };
	println!("cargo:rustc-env=GIT_HASH={git_hash}");
	println!("cargo:rustc-env=CSS_HASH={:016x}", css_hash());
	println!("cargo:rustc-env=UPSTREAM_AHEAD={}", std::env::var("REDLIB_BUILD_UPSTREAM_AHEAD").unwrap_or_default());
	println!("cargo:rustc-env=UPSTREAM_BEHIND={}", std::env::var("REDLIB_BUILD_UPSTREAM_BEHIND").unwrap_or_default());
}

fn css_hash() -> u64 {
	let mut files = vec![PathBuf::from("static/style.css")];
	if let Ok(entries) = fs::read_dir("static/themes") {
		files.extend(
			entries
				.flatten()
				.map(|entry| entry.path())
				.filter(|path| path.extension().is_some_and(|extension| extension == "css")),
		);
	}
	files.sort();

	let mut hash = 0xcbf29ce484222325_u64;
	for path in files {
		hash_bytes(&mut hash, path.to_string_lossy().as_bytes());
		if let Ok(contents) = fs::read(&path) {
			hash_bytes(&mut hash, &contents);
		}
	}
	hash
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
	for byte in bytes {
		*hash ^= u64::from(*byte);
		*hash = hash.wrapping_mul(0x100000001b3);
	}
}
