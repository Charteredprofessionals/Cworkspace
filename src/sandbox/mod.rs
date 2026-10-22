pub mod path_jail;
pub mod win_job;

use std::io;
use std::path::PathBuf;

pub fn register_session_jail(
	session_id: impl Into<String>,
	roots: Vec<PathBuf>,
) -> io::Result<crate::tool::SessionPathValidatorRegistration> {
	let jail = path_jail::PathJail::new(roots)?;
	Ok(crate::tool::register_session_path_validator(
		session_id,
		move |path| jail.verify_path(path),
	))
}