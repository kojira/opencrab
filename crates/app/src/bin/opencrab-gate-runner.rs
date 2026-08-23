//! Read canonical instance config once, then replace this process with the selected gate adapter.

use base64::Engine as _;
use opencrab_port::GateInstanceId;
use opencrab_store::{gate_launch_read_only, GateLaunchSecret};
use ring::aead;
use std::os::unix::process::CommandExt as _;
use std::process::Command;
use zeroize::{Zeroize, Zeroizing};

const MASTER_KEY_ENV: &str = "OPENCRAB_SECRET_MASTER_KEY";
const RUNNER_OWNED_ENVS: [&str; 8] = [
    MASTER_KEY_ENV,
    "OPENCRAB_GATE_TOKEN",
    "OPENCRAB_GATE_BOOT_ERROR_CODE",
    "OPENCRAB_GATE_INSTANCE_ID",
    "OPENCRAB_GATE_KIND_ID",
    "OPENCRAB_GATE_REVISION",
    "OPENCRAB_GATE_SOCKET",
    "OPENCRAB_GATE_CONFIG_SCHEMA",
];
const GATE_CONFIG_B64_ENV: &str = "OPENCRAB_GATE_CONFIG_B64";

fn usage() -> ! {
    panic!("usage: opencrab-gate-runner <db> <instance-uuid> <core-socket> <adapter> [args...]")
}

fn quarter_round(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(7);
}

fn hchacha20(key: &[u8; 32], nonce: &[u8; 16]) -> [u8; 32] {
    let mut state = [0_u32; 16];
    state[..4].copy_from_slice(&[0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]);
    // Split into 4-byte chunks matching the width of u32.
    for (index, chunk) in key.as_chunks::<4>().0.iter().enumerate() {
        state[4 + index] = u32::from_le_bytes(*chunk);
    }
    for (index, chunk) in nonce.as_chunks::<4>().0.iter().enumerate() {
        state[12 + index] = u32::from_le_bytes(*chunk);
    }
    for _ in 0..10 {
        quarter_round(&mut state, 0, 4, 8, 12);
        quarter_round(&mut state, 1, 5, 9, 13);
        quarter_round(&mut state, 2, 6, 10, 14);
        quarter_round(&mut state, 3, 7, 11, 15);
        quarter_round(&mut state, 0, 5, 10, 15);
        quarter_round(&mut state, 1, 6, 11, 12);
        quarter_round(&mut state, 2, 7, 8, 13);
        quarter_round(&mut state, 3, 4, 9, 14);
    }
    let words = [
        state[0], state[1], state[2], state[3], state[12], state[13], state[14], state[15],
    ];
    let mut out = [0_u8; 32];
    for (index, word) in words.into_iter().enumerate() {
        out[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    state.zeroize();
    out
}

fn parse_master_key(value: &str) -> Result<Zeroizing<[u8; 32]>, ()> {
    let decoded = Zeroizing::new(
        base64::engine::general_purpose::STANDARD
            .decode(value.trim())
            .map_err(|_| ())?,
    );
    let mut key = Zeroizing::new([0_u8; 32]);
    if decoded.len() != key.len() {
        return Err(());
    }
    key.copy_from_slice(&decoded);
    Ok(key)
}

fn decrypt_v1(value: &[u8], master: &[u8; 32]) -> Result<Zeroizing<Vec<u8>>, ()> {
    let value = std::str::from_utf8(value).map_err(|_| ())?;
    let encoded = value.strip_prefix("enc:v1:").ok_or(())?;
    let mut blob = Zeroizing::new(
        base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|_| ())?,
    );
    if blob.len() < 24 + aead::CHACHA20_POLY1305.tag_len() {
        return Err(());
    }
    let nonce_prefix: [u8; 16] = blob[..16].try_into().map_err(|_| ())?;
    let nonce_tail: [u8; 8] = blob[16..24].try_into().map_err(|_| ())?;
    let mut subkey = Zeroizing::new(hchacha20(master, &nonce_prefix));
    let unbound = aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &subkey[..]).map_err(|_| ())?;
    subkey.zeroize();
    let key = aead::LessSafeKey::new(unbound);
    let mut nonce = [0_u8; 12];
    nonce[4..].copy_from_slice(&nonce_tail);
    blob.drain(..24);
    let plain = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::empty(),
            &mut blob,
        )
        .map_err(|_| ())?;
    Ok(Zeroizing::new(plain.to_vec()))
}

fn resolve_secret(
    secret: &GateLaunchSecret,
    master: Option<&[u8; 32]>,
) -> Result<Zeroizing<Vec<u8>>, &'static str> {
    match secret.at_rest_format.as_str() {
        "source-plaintext" => Ok(Zeroizing::new(secret.value.clone())),
        "enc:v1" => decrypt_v1(&secret.value, master.ok_or("master_key_missing")?)
            .map_err(|_| "secret_decrypt_failed"),
        "opaque" => Err("secret_format_opaque"),
        _ => Err("secret_format_unknown"),
    }
}

fn adapter_command(adapter: std::ffi::OsString, adapter_args: Vec<std::ffi::OsString>) -> Command {
    let mut command = Command::new(adapter);
    command.args(adapter_args);
    for name in RUNNER_OWNED_ENVS {
        command.env_remove(name);
    }
    command.env_remove(GATE_CONFIG_B64_ENV);
    command
}

fn set_boot_error(command: &mut Command, code: &'static str) {
    command
        .env_remove("OPENCRAB_GATE_TOKEN")
        .env("OPENCRAB_GATE_BOOT_ERROR_CODE", code);
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let db = args.next().unwrap_or_else(|| usage());
    let instance = args.next().unwrap_or_else(|| usage());
    let socket = args.next().unwrap_or_else(|| usage());
    let adapter = args.next().unwrap_or_else(|| usage());
    let adapter_args: Vec<_> = args.collect();
    let instance_text = instance.to_string_lossy().into_owned();
    let instance = GateInstanceId::parse(instance_text).unwrap_or_else(|_| usage());
    let master_value = std::env::var(MASTER_KEY_ENV).ok().map(Zeroizing::new);
    // This single-threaded runner reads the inherited value once, then removes it before exec.
    unsafe { std::env::remove_var(MASTER_KEY_ENV) };
    let master = master_value
        .as_deref()
        .and_then(|value| parse_master_key(value).ok());

    let launch = gate_launch_read_only(&db, &instance).ok().flatten();
    let mut boot_error = None;
    let mut token = Zeroizing::new(Vec::new());
    let mut command = adapter_command(adapter, adapter_args);

    if let Some(launch) = launch {
        command
            .env("OPENCRAB_GATE_INSTANCE_ID", launch.instance_id.as_str())
            .env("OPENCRAB_GATE_KIND_ID", launch.kind_id.as_str())
            .env("OPENCRAB_GATE_REVISION", launch.revision.to_string())
            .env("OPENCRAB_GATE_SOCKET", socket)
            .env("OPENCRAB_GATE_CONFIG_SCHEMA", launch.config_schema_id)
            .env(
                GATE_CONFIG_B64_ENV,
                base64::engine::general_purpose::STANDARD.encode(launch.config_bytes),
            );
        let expected = match launch.kind_id.as_str() {
            "discord" => "discord_bot_token",
            "nostr" => "nostr_secret_key",
            _ => "",
        };
        if !expected.is_empty() {
            if let Some(secret) = launch.secrets.iter().find(|secret| secret.name == expected) {
                match resolve_secret(secret, master.as_deref()) {
                    Ok(value) if !value.contains(&0) => token = value,
                    Ok(_) => boot_error = Some("secret_contains_nul"),
                    Err(code) => boot_error = Some(code),
                }
            } else {
                boot_error = Some("secret_missing");
            }
        }
    } else {
        boot_error = Some("instance_config_unavailable");
        command
            .env("OPENCRAB_GATE_INSTANCE_ID", instance.as_str())
            .env("OPENCRAB_GATE_SOCKET", socket);
    }

    if let Some(code) = boot_error {
        set_boot_error(&mut command, code);
    } else {
        use std::os::unix::ffi::OsStringExt as _;
        command.env(
            "OPENCRAB_GATE_TOKEN",
            std::ffi::OsString::from_vec(token.to_vec()),
        );
    }
    token.zeroize();
    let error = command.exec();
    panic!("could not exec gate adapter: {error}");
}

#[cfg(test)]
mod tests {
    use super::{
        adapter_command, hchacha20, set_boot_error, GATE_CONFIG_B64_ENV, RUNNER_OWNED_ENVS,
    };
    use std::ffi::OsStr;

    #[test]
    fn hchacha20_matches_draft_vector() {
        let key: [u8; 32] = std::array::from_fn(|index| index as u8);
        let nonce = [
            0, 0, 0, 9, 0, 0, 0, 0x4a, 0, 0, 0, 0, 0x31, 0x41, 0x59, 0x27,
        ];
        assert_eq!(
            hchacha20(&key, &nonce),
            [
                0x82, 0x41, 0x3b, 0x42, 0x27, 0xb2, 0x7b, 0xfe, 0xd3, 0x0e, 0x42, 0x50, 0x8a, 0x87,
                0x7d, 0x73, 0xa0, 0xf9, 0xe4, 0xd5, 0x8a, 0x74, 0xa8, 0x53, 0xc1, 0x2e, 0xc4, 0x13,
                0x26, 0xd3, 0xec, 0xdc,
            ]
        );
    }

    #[test]
    fn boot_error_child_has_no_inherited_runner_state_or_token() {
        let mut command = adapter_command("synthetic-adapter".into(), Vec::new());
        set_boot_error(&mut command, "instance_config_unavailable");

        let envs: std::collections::BTreeMap<_, _> = command
            .get_envs()
            .map(|(name, value)| (name.to_os_string(), value.map(OsStr::to_os_string)))
            .collect();
        assert_eq!(envs.get(OsStr::new("OPENCRAB_GATE_TOKEN")), Some(&None));
        assert_eq!(
            envs.get(OsStr::new("OPENCRAB_GATE_BOOT_ERROR_CODE")),
            Some(&Some("instance_config_unavailable".into()))
        );
        for name in RUNNER_OWNED_ENVS {
            if name != "OPENCRAB_GATE_BOOT_ERROR_CODE" {
                assert_eq!(envs.get(OsStr::new(name)), Some(&None), "{name}");
            }
        }
        assert_eq!(envs.get(OsStr::new(GATE_CONFIG_B64_ENV)), Some(&None));
    }
}
