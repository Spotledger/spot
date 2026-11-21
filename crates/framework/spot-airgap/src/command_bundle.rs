//! Signed command bundles: one or more administrator commands carried in on media.
//!
//! An air-gapped agent cannot reach the console, so an administrator's signed commands travel
//! on removable media as a [`CommandBundle`]. The bundle carries, for each command, the
//! command's canonical bytes and one detached Ed25519 signature per signer, each with the
//! signer's public key so the reader can name the key the same way the verifier does. Nothing
//! here decides whether a command runs: the agent's [`CommandVerifier`] does, with its own
//! trust list, policy and nonce window — and because the nonce window records each accepted
//! command, a bundle replayed on the agent is refused with `cmd.replay`.
//!
//! The command layer (`spot-signed-command`) has no wire form for its envelope, so this module
//! writes one and reconstructs the envelope exactly: the canonical command bytes are parsed by
//! `Command::from_json_str` (which re-checks canonicity) and each signature is rebuilt from its
//! raw 64 bytes and the key id derived from its public key.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};
use spot_mgmt_ops::OpId;
use spot_signed_command::{
    Command, CommandSignature, CommandVerifier, KeyId, SignedCommand, Verified,
};

use crate::error::AirgapError;

/// The `format` field of every command bundle document.
pub const COMMAND_BUNDLE_FORMAT: &str = "spot-command-bundle";
/// The format version this module writes.
pub const COMMAND_BUNDLE_VERSION: u32 = 1;

/// One signature a command carries: the signer's public key and the raw signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundledSignature {
    /// The signer's 32-byte Ed25519 public key.
    pub public_key: [u8; 32],
    /// The 64-byte Ed25519 signature over the command's signed message.
    pub signature: [u8; 64],
}

/// One command and its signatures, ready to verify.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundledCommand {
    /// The signed content.
    pub command: Command,
    /// The signatures over it.
    pub signatures: Vec<BundledSignature>,
}

/// A set of signed commands carried to an air-gapped agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandBundle {
    /// The commands, in the order they were packed.
    pub commands: Vec<BundledCommand>,
}

/// The JSON shape of a command bundle.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleDoc {
    format: String,
    version: u32,
    commands: Vec<CommandDoc>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandDoc {
    command: String,
    signatures: Vec<SignatureDoc>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignatureDoc {
    public_key: String,
    signature: String,
}

impl BundledCommand {
    /// Builds a bundled command from a signed one. `public_keys` maps a signer's key id text to
    /// its public key bytes; a signature whose key is not in the map is refused.
    ///
    /// # Errors
    /// [`AirgapError::CommandBundleMalformed`] when a signature's key is unknown.
    pub fn from_signed(
        signed: &SignedCommand,
        public_keys: &BTreeMap<String, [u8; 32]>,
    ) -> Result<Self, AirgapError> {
        let mut signatures = Vec::with_capacity(signed.signatures.len());
        for entry in &signed.signatures {
            let public_key = public_keys
                .get(entry.key_id.as_str())
                .copied()
                .ok_or_else(|| AirgapError::CommandBundleMalformed {
                    detail: format!(
                        "no public key was supplied for signer {}",
                        entry.key_id.as_str()
                    ),
                })?;
            signatures.push(BundledSignature {
                public_key,
                signature: entry.signature.to_bytes(),
            });
        }
        Ok(Self {
            command: signed.command.clone(),
            signatures,
        })
    }

    /// Rebuilds the signed-command envelope this bundled command describes.
    ///
    /// # Errors
    /// [`AirgapError::CommandBundleMalformed`] when the command has no canonical bytes.
    pub fn to_signed(&self) -> Result<SignedCommand, AirgapError> {
        let signatures = self
            .signatures
            .iter()
            .map(|entry| CommandSignature {
                key_id: KeyId::from_public_key(&entry.public_key),
                signature: Signature::from_bytes(&entry.signature),
            })
            .collect();
        Ok(SignedCommand {
            command: self.command.clone(),
            signatures,
        })
    }

    /// Verifies this command with the agent's verifier, which also records its nonce so a
    /// replay is refused.
    ///
    /// # Errors
    /// [`AirgapError::CommandRejected`] naming the command layer's code.
    pub async fn verify(
        &self,
        verifier: &CommandVerifier,
        audience: &str,
        now: i64,
    ) -> Result<Verified, AirgapError> {
        Ok(verifier.verify(&self.to_signed()?, audience, now).await?)
    }
}

impl CommandBundle {
    /// Packs signed commands into the bundle's JSON bytes.
    ///
    /// # Errors
    /// [`AirgapError::CommandBundleMalformed`] when a signer's public key is missing or a
    /// command has no canonical bytes.
    pub fn pack(
        commands: &[SignedCommand],
        public_keys: &BTreeMap<String, [u8; 32]>,
    ) -> Result<Vec<u8>, AirgapError> {
        let mut docs = Vec::with_capacity(commands.len());
        for signed in commands {
            let bundled = BundledCommand::from_signed(signed, public_keys)?;
            let bytes = bundled.command.canonical_bytes().map_err(|error| {
                AirgapError::CommandBundleMalformed {
                    detail: error.to_string(),
                }
            })?;
            let command =
                String::from_utf8(bytes).map_err(|error| AirgapError::CommandBundleMalformed {
                    detail: error.to_string(),
                })?;
            docs.push(CommandDoc {
                command,
                signatures: bundled
                    .signatures
                    .iter()
                    .map(|entry| SignatureDoc {
                        public_key: STANDARD.encode(entry.public_key),
                        signature: STANDARD.encode(entry.signature),
                    })
                    .collect(),
            });
        }
        let doc = BundleDoc {
            format: COMMAND_BUNDLE_FORMAT.to_owned(),
            version: COMMAND_BUNDLE_VERSION,
            commands: docs,
        };
        serde_json::to_vec(&doc).map_err(|error| AirgapError::CommandBundleMalformed {
            detail: error.to_string(),
        })
    }

    /// Reads a bundle from its JSON bytes, checking every field and re-parsing each command.
    ///
    /// # Errors
    /// [`AirgapError::CommandBundleMalformed`] when the document does not parse, the format or
    /// version is wrong, a command is not canonical, or a key or signature is malformed.
    pub fn unpack(bytes: &[u8]) -> Result<Self, AirgapError> {
        let doc: BundleDoc =
            serde_json::from_slice(bytes).map_err(|error| AirgapError::CommandBundleMalformed {
                detail: error.to_string(),
            })?;
        if doc.format != COMMAND_BUNDLE_FORMAT {
            return Err(AirgapError::CommandBundleMalformed {
                detail: format!(
                    "the format is `{}`, not `{COMMAND_BUNDLE_FORMAT}`",
                    doc.format
                ),
            });
        }
        if doc.version != COMMAND_BUNDLE_VERSION {
            return Err(AirgapError::CommandBundleMalformed {
                detail: format!("version {} is not supported", doc.version),
            });
        }
        let commands = doc
            .commands
            .into_iter()
            .map(|command| {
                let parsed = Command::from_json_str(&command.command).map_err(|error| {
                    AirgapError::CommandBundleMalformed {
                        detail: error.to_string(),
                    }
                })?;
                let signatures = command
                    .signatures
                    .into_iter()
                    .map(parse_signature)
                    .collect::<Result<Vec<_>, AirgapError>>()?;
                Ok(BundledCommand {
                    command: parsed,
                    signatures,
                })
            })
            .collect::<Result<Vec<_>, AirgapError>>()?;
        Ok(Self { commands })
    }

    /// The operation ids the bundle's commands name, checked against the management catalogue's
    /// naming rule. The agent runs only operations it knows; a malformed name is refused here.
    ///
    /// # Errors
    /// [`AirgapError::CommandBundleMalformed`] when a command names an operation that is not a
    /// dotted lower-case id of two to four segments.
    pub fn operations(&self) -> Result<Vec<OpId>, AirgapError> {
        self.commands
            .iter()
            .map(|bundled| {
                OpId::new(bundled.command.op()).ok_or_else(|| AirgapError::CommandBundleMalformed {
                    detail: format!(
                        "`{}` is not a management operation id",
                        bundled.command.op()
                    ),
                })
            })
            .collect()
    }
}

/// Parses one signature document entry.
fn parse_signature(doc: SignatureDoc) -> Result<BundledSignature, AirgapError> {
    let public_bytes =
        STANDARD
            .decode(&doc.public_key)
            .map_err(|error| AirgapError::CommandBundleMalformed {
                detail: error.to_string(),
            })?;
    let public_key = <[u8; 32]>::try_from(public_bytes.as_slice()).map_err(|_| {
        AirgapError::CommandBundleMalformed {
            detail: "a public key is 32 bytes".to_owned(),
        }
    })?;
    let signature_bytes =
        STANDARD
            .decode(&doc.signature)
            .map_err(|error| AirgapError::CommandBundleMalformed {
                detail: error.to_string(),
            })?;
    let signature = <[u8; 64]>::try_from(signature_bytes.as_slice()).map_err(|_| {
        AirgapError::CommandBundleMalformed {
            detail: "a signature is 64 bytes".to_owned(),
        }
    })?;
    Ok(BundledSignature {
        public_key,
        signature,
    })
}
