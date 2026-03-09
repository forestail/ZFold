use crate::cli::PasswordSourceArgs;
use anyhow::{anyhow, bail, ensure, Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{
    aead::{rand_core::RngCore, Aead, KeyInit, OsRng},
    ChaCha20Poly1305, Key, Nonce,
};
use rpassword::prompt_password;
use std::fs;

pub const FLAG_ENCRYPTED: u16 = 0x0002;
pub const SALT_LEN: usize = 16;
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const ARGON_M_COST_KIB: u32 = 64 * 1024;
const ARGON_T_COST: u32 = 3;
const ARGON_P_COST: u32 = 1;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SectionKind {
    Chunk,
    Dictionary,
    Index,
}

pub type EncryptionKey = [u8; KEY_LEN];

pub fn password_source_present(args: &PasswordSourceArgs) -> bool {
    args.password.is_some() || args.password_env.is_some() || args.password_file.is_some()
}

pub fn resolve_pack_password(
    args: &PasswordSourceArgs,
    password_prompt: bool,
) -> Result<Option<String>> {
    if !password_prompt && !password_source_present(args) {
        return Ok(None);
    }

    let password = load_password(args, "New archive password: ", true)?;
    Ok(Some(password))
}

pub fn resolve_archive_password(
    args: &PasswordSourceArgs,
    encrypted: bool,
) -> Result<Option<String>> {
    if encrypted {
        return load_password(args, "Archive password: ", false).map(Some);
    }

    ensure!(
        !password_source_present(args),
        "password options were provided, but the archive is not encrypted"
    );
    Ok(None)
}

pub fn generate_salt() -> [u8; SALT_LEN] {
    let mut salt = [0u8; SALT_LEN];
    let mut rng = OsRng;
    rng.fill_bytes(&mut salt);
    salt
}

pub fn derive_key(password: &str, salt: &[u8; SALT_LEN]) -> Result<EncryptionKey> {
    ensure!(!password.is_empty(), "password must not be empty");

    let params = Params::new(ARGON_M_COST_KIB, ARGON_T_COST, ARGON_P_COST, Some(KEY_LEN))
        .map_err(|err| anyhow!("failed to create Argon2 parameters: {err}"))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; KEY_LEN];
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|err| anyhow!("failed to derive encryption key from password: {err}"))?;
    Ok(key)
}

pub fn encrypt_section(
    plaintext: &[u8],
    key: &EncryptionKey,
    section: SectionKind,
    id: u64,
) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let nonce_bytes = nonce_for(section, id);
    let nonce = Nonce::from_slice(&nonce_bytes);

    cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| anyhow!("failed to encrypt archive section"))
}

pub fn decrypt_section(
    ciphertext: &[u8],
    key: &EncryptionKey,
    section: SectionKind,
    id: u64,
) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let nonce_bytes = nonce_for(section, id);
    let nonce = Nonce::from_slice(&nonce_bytes);

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| anyhow!("failed to decrypt archive section"))
}

fn load_password(args: &PasswordSourceArgs, prompt: &str, confirm: bool) -> Result<String> {
    let (mut password, prompted) = match (&args.password, &args.password_env, &args.password_file) {
        (Some(password), None, None) => (password.clone(), false),
        (None, Some(var), None) => (
            std::env::var(var).with_context(|| format!("environment variable {var} is not set"))?,
            false,
        ),
        (None, None, Some(path)) => (
            fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?,
            false,
        ),
        (None, None, None) => (
            prompt_password(prompt).context("failed to read password from terminal")?,
            true,
        ),
        _ => bail!("at most one password source may be specified"),
    };

    if args.password_file.is_some() {
        trim_password_trailing_newlines(&mut password);
    }
    ensure!(!password.is_empty(), "password must not be empty");

    if confirm && prompted {
        let confirm_password = prompt_password("Confirm archive password: ")
            .context("failed to read password confirmation")?;
        ensure!(
            password == confirm_password,
            "password confirmation did not match"
        );
    }

    Ok(password)
}

fn trim_password_trailing_newlines(password: &mut String) {
    while matches!(password.as_bytes().last(), Some(b'\n' | b'\r')) {
        password.pop();
    }
}

fn nonce_for(section: SectionKind, id: u64) -> [u8; NONCE_LEN] {
    let section_id = match section {
        SectionKind::Chunk => 1u32,
        SectionKind::Dictionary => 2u32,
        SectionKind::Index => 3u32,
    };

    let mut nonce = [0u8; NONCE_LEN];
    nonce[..4].copy_from_slice(&section_id.to_le_bytes());
    nonce[4..].copy_from_slice(&id.to_le_bytes());
    nonce
}

#[cfg(test)]
mod tests {
    use super::{decrypt_section, derive_key, encrypt_section, SectionKind};
    use anyhow::Result;

    #[test]
    fn section_encryption_roundtrip() -> Result<()> {
        let salt = [7u8; 16];
        let key = derive_key("secret-password", &salt)?;
        let plaintext = b"very secret chunk bytes";

        let encrypted = encrypt_section(plaintext, &key, SectionKind::Chunk, 42)?;
        let decrypted = decrypt_section(&encrypted, &key, SectionKind::Chunk, 42)?;

        assert_eq!(decrypted, plaintext);
        Ok(())
    }
}
