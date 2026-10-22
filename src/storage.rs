use anyhow::{Context, Result};
use keyring::Entry;
use std::path::{Path, PathBuf};

const SERVICE: &str = "reia-local-assistant";
const USER: &str = "knowledge-database-key";

pub fn get_secret(name: &str) -> Result<Option<String>> {
    let entry = Entry::new(SERVICE, name).context("could not access the OS keyring")?;
    match entry.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(error).context("could not read a credential from the OS keyring"),
    }
}

pub fn set_secret(name: &str, secret: &str) -> Result<()> {
    Entry::new(SERVICE, name)
        .context("could not access the OS keyring")?
        .set_password(secret)
        .context("could not save a credential to the OS keyring")
}

pub fn secret_configured(name: &str) -> Result<bool> {
    Ok(get_secret(name)?.is_some())
}

pub fn data_dir() -> Result<PathBuf> {
    let path = dirs::data_local_dir()
        .context("could not locate the local application data directory")?
        .join("Reia");
    std::fs::create_dir_all(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(path)
}

pub fn config_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("could not locate the application configuration directory")?
        .join("Reia")
        .join("config.toml"))
}

pub fn database_key() -> Result<Vec<u8>> {
    let entry = Entry::new(SERVICE, USER).context("could not access the OS keyring")?;
    match entry.get_password() {
        Ok(key) => hex::decode(key).context("the database key in the OS keyring is invalid"),
        Err(keyring::Error::NoEntry) => {
            let mut bytes = [0_u8; 32];
            rand::RngCore::fill_bytes(&mut rand::rng(), &mut bytes);
            let key = hex::encode(bytes);
            entry
                .set_password(&key)
                .context("could not save the local database key to the OS keyring")?;
            Ok(bytes.to_vec())
        }
        Err(error) => Err(error).context("could not read the local database key"),
    }
}

pub fn ensure_private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
