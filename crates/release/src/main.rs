//! `cairn-release`: the publisher's side of updates (`cairn_protocol::update`). It makes the Ed25519
//! key releases are signed with, and signs a release's `update.json` from its files. The secret key is
//! read from the environment, `CAIRN_UPDATE_SIGNING_KEY`, never from the command line, which any user
//! of the machine can read.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use cairn_protocol::update::{
    Form, Release, ReleaseFile, Signature, Signed, Version, SIGNED_PREFIX,
};
use clap::{Parser, Subcommand};
use ring::signature::{Ed25519KeyPair, KeyPair};
use std::io::Read;
use std::path::{Path, PathBuf};

const SECRET_VAR: &str = "CAIRN_UPDATE_SIGNING_KEY";

#[derive(Parser)]
#[command(
    name = "cairn-release",
    version,
    about = "Sign a Cairn release's update.json"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Make a new signing key: its secret into a new file, readable by this user alone, and its
    /// public key on stdout, for builds to trust (CAIRN_UPDATE_KEYS).
    Keygen { secret: PathBuf },
    /// The public key of the secret in CAIRN_UPDATE_SIGNING_KEY.
    Public,
    /// Write update.json for these files, signed with the key in CAIRN_UPDATE_SIGNING_KEY.
    Sign {
        /// The release's version, as each program's --version says it.
        #[arg(long)]
        version: String,
        /// Where its files can be downloaded, each by its file name under it. Default: beside
        /// update.json, wherever that is fetched from.
        #[arg(long)]
        base_url: Option<String>,
        /// Where a person reads what is in it.
        #[arg(long, default_value = "")]
        notes: String,
        #[arg(long)]
        out: PathBuf,
        /// Each `program:platform:form:path`: `cairn:linux-x86_64:binary:dist/cairn-0.1.6-linux-x86_64`.
        #[arg(required = true)]
        files: Vec<String>,
    },
    /// Check update.json against a public key and print the release in it.
    Verify {
        #[arg(long = "key", required = true)]
        keys: Vec<String>,
        file: PathBuf,
    },
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn secret() -> Result<Ed25519KeyPair> {
    let text = std::env::var(SECRET_VAR).map_err(|_| anyhow!("{SECRET_VAR} is not set"))?;
    let der = b64()
        .decode(text.trim())
        .with_context(|| format!("{SECRET_VAR} is not base64"))?;
    Ed25519KeyPair::from_pkcs8(&der)
        .map_err(|e| anyhow!("{SECRET_VAR} is not an Ed25519 key in PKCS#8: {e}"))
}

fn sha256_of(path: &Path) -> Result<(u64, String)> {
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 1 << 16];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        ctx.update(&buf[..n]);
    }
    Ok((size, hex::encode(ctx.finish().as_ref())))
}

fn file_of(spec: &str, base_url: Option<&str>) -> Result<ReleaseFile> {
    let mut parts = spec.splitn(4, ':');
    let (Some(program), Some(platform), Some(form), Some(path)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        bail!("{spec}: not program:platform:form:path");
    };
    if !matches!(program, "cairn" | "cairn-tray") {
        bail!("{spec}: the program is cairn or cairn-tray");
    }
    let form = match form {
        "binary" => Form::Binary,
        "app" => Form::App,
        _ => bail!("{spec}: the form is binary or app"),
    };
    let path = Path::new(path);
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{spec}: no file name"))?
        .to_string_lossy()
        .into_owned();
    let (size, sha256) = sha256_of(path)?;
    let url = match base_url {
        Some(base) => format!("{}/{name}", base.trim_end_matches('/')),
        None => name,
    };
    Ok(ReleaseFile {
        program: program.into(),
        platform: platform.into(),
        form,
        url,
        size,
        sha256,
    })
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Keygen { secret } => {
            let rng = ring::rand::SystemRandom::new();
            let der = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|e| anyhow!("{e}"))?;
            let pair = Ed25519KeyPair::from_pkcs8(der.as_ref()).map_err(|e| anyhow!("{e}"))?;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
            let mut f = opts
                .open(&secret)
                .with_context(|| format!("make {}", secret.display()))?;
            std::io::Write::write_all(
                &mut f,
                format!("{}\n", b64().encode(der.as_ref())).as_bytes(),
            )?;
            f.sync_all()?;
            println!("{}", b64().encode(pair.public_key().as_ref()));
        }
        Command::Public => println!("{}", b64().encode(secret()?.public_key().as_ref())),
        Command::Sign {
            version,
            base_url,
            notes,
            out,
            files,
        } => {
            if Version::parse(&version).is_none() {
                bail!("{version} is not a version");
            }
            let pair = secret()?;
            let files = files
                .iter()
                .map(|f| file_of(f, base_url.as_deref()))
                .collect::<Result<Vec<_>>>()?;
            let release = Release {
                version,
                protocol: cairn_protocol::PROTOCOL_VERSION,
                published: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs() as i64,
                notes,
                files,
            };
            let text = serde_json::to_string_pretty(&release)?;
            let mut message = SIGNED_PREFIX.to_vec();
            message.extend_from_slice(text.as_bytes());
            let signed = Signed {
                release: text,
                signatures: vec![Signature {
                    key: b64().encode(pair.public_key().as_ref()),
                    signature: b64().encode(pair.sign(&message).as_ref()),
                }],
            };
            std::fs::write(&out, serde_json::to_vec_pretty(&signed)?)
                .with_context(|| format!("write {}", out.display()))?;
            eprintln!(
                "{}: {} files, signed with {}",
                out.display(),
                release_files(&signed)?,
                signed.signatures[0].key
            );
        }
        Command::Verify { keys, file } => {
            let bytes = std::fs::read(&file).with_context(|| format!("read {}", file.display()))?;
            let signed: Signed = serde_json::from_slice(&bytes)?;
            let mut message = SIGNED_PREFIX.to_vec();
            message.extend_from_slice(signed.release.as_bytes());
            let good = signed.signatures.iter().any(|s| {
                keys.contains(&s.key)
                    && match (b64().decode(&s.key), b64().decode(&s.signature)) {
                        (Ok(k), Ok(sig)) => {
                            ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, k)
                                .verify(&message, &sig)
                                .is_ok()
                        }
                        _ => false,
                    }
            });
            if !good {
                bail!("{}: not signed by any of these keys", file.display());
            }
            println!("{}", signed.release);
        }
    }
    Ok(())
}

fn release_files(signed: &Signed) -> Result<usize> {
    Ok(serde_json::from_str::<Release>(&signed.release)?
        .files
        .len())
}
