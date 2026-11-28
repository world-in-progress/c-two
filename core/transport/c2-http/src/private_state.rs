use std::{
    fs, io,
    path::{Path, PathBuf},
};

use c2_local_security::{create_private_dir, verify_private_file, write_private_file};

pub(crate) fn state_dir(path: Option<&Path>, default: &str) -> Result<PathBuf, String> {
    let dir = match path {
        Some(path) => path.to_owned(),
        None => std::env::home_dir()
            .ok_or("cannot find the home directory; specify --state-dir")?
            .join(default),
    };
    create_private_dir(&dir).map_err(|error| file_error(&dir, error))?;
    Ok(dir)
}

pub(crate) fn persistent_hex32(dir: &Path, name: &str) -> Result<[u8; 32], String> {
    let path = dir.join(name);
    let mut bytes = [0; 32];
    if private_file_exists(&path)? {
        let value = fs::read(&path).map_err(|error| file_error(&path, error))?;
        if value.len() != 64
            || !value
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        {
            return Err(format!(
                "{} must contain 64 lowercase hex digits",
                path.display()
            ));
        }
        hex::decode_to_slice(value, &mut bytes).expect("validated lowercase hex");
    } else {
        getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
        write_private_file(&path, hex::encode(bytes).as_bytes())
            .map_err(|error| file_error(&path, error))?;
        sync_directory(dir)?;
    }
    Ok(bytes)
}

pub(crate) fn persistent_tls_identity(
    dir: &Path,
    stem: &str,
    names: Vec<String>,
) -> Result<(PathBuf, PathBuf), String> {
    let cert = dir.join(format!("{stem}-cert.pem"));
    let key = dir.join(format!("{stem}-key.pem"));
    match (private_file_exists(&cert)?, private_file_exists(&key)?) {
        (true, true) => {}
        (false, false) => {
            let identity = rcgen::generate_simple_self_signed(names)
                .map_err(|error| format!("generate {stem} TLS identity: {error}"))?;
            write_private_file(&key, identity.signing_key.serialize_pem().as_bytes())
                .map_err(|error| file_error(&key, error))?;
            if let Err(error) = write_private_file(&cert, identity.cert.pem().as_bytes()) {
                let _ = fs::remove_file(&key);
                return Err(file_error(&cert, error));
            }
            sync_directory(dir)?;
        }
        _ => {
            return Err(format!(
                "incomplete {stem} TLS identity in {}",
                dir.display()
            ));
        }
    }
    Ok((cert, key))
}

fn private_file_exists(path: &Path) -> Result<bool, String> {
    match verify_private_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(file_error(path, error)),
    }
}

fn sync_directory(_dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    fs::File::open(_dir)
        .and_then(|dir| dir.sync_all())
        .map_err(|error| file_error(_dir, error))?;
    Ok(())
}

fn file_error(path: &Path, error: io::Error) -> String {
    format!("state {}: {error}", path.display())
}

pub(crate) fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    // Keep each input comparison independent of the accumulated difference.
    let mut difference = 0;
    for (a, b) in a.iter().zip(b) {
        difference |= std::hint::black_box(a ^ b);
    }
    difference == 0
}
