use rsa::{
    pkcs8::{EncodePrivateKey, LineEnding},
    rand_core::OsRng,
    RsaPrivateKey,
};
use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

pub(crate) fn adb_key_path(app_data_dir: &Path) -> io::Result<PathBuf> {
    let user_home = env::var_os("ANDROID_USER_HOME").filter(|path| !path.is_empty());
    let sdk_keys = user_home
        .into_iter()
        .flat_map(|home| {
            let home = PathBuf::from(home);
            [home.join("adbkey"), home.join("android").join("adbkey")]
        })
        .chain(env::home_dir().map(|home| home.join(".android").join("adbkey")));

    for path in sdk_keys {
        if path.try_exists()? {
            return Ok(path);
        }
    }

    persistent_app_key(app_data_dir)
}

fn persistent_app_key(app_data_dir: &Path) -> io::Result<PathBuf> {
    let path = app_data_dir.join("adbkey");
    if path.try_exists()? {
        return Ok(path);
    }

    fs::create_dir_all(app_data_dir)?;
    let pem = RsaPrivateKey::new(&mut OsRng, 2048)
        .map_err(io::Error::other)?
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(io::Error::other)?;
    let mut temporary_key = tempfile::NamedTempFile::new_in(app_data_dir)?;
    temporary_key.write_all(pem.as_bytes())?;
    temporary_key.as_file().sync_all()?;

    match temporary_key.persist_noclobber(&path) {
        Ok(_) => Ok(path),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(path),
        Err(error) => Err(error.error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs8::DecodePrivateKey;

    #[test]
    fn reuses_the_private_key_across_connections() {
        let directory = tempfile::tempdir().unwrap();
        let app_data_dir = directory.path().join("app-data");
        let path = persistent_app_key(&app_data_dir).unwrap();
        let first_key = RsaPrivateKey::read_pkcs8_pem_file(&path).unwrap();

        let next_path = persistent_app_key(&app_data_dir).unwrap();
        let next_key = RsaPrivateKey::read_pkcs8_pem_file(next_path).unwrap();

        assert_eq!(first_key, next_key);
    }
}
