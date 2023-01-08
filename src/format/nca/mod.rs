//! NCA Parsing
//!
//! Nintendo Container Archives (NCAs) are signed and encrypted archives that
//! contain software and other content Nintendo provides. Almost every file on
//! the Horizon/NX OS are stored in this container, as it guarantees its
//! authenticity, preventing tampering.
//!
//! For more information about the NCA file format, see the [switchbrew page].
//!
//! In order to parse an NCA, you may use the `from_file` method:
//!
//! ```
//! # fn get_nca_file() -> std::io::Result<std::fs::File> {
//! #   std::fs::File::open("tests/fixtures/test.nca")
//! # }
//! let f = get_nca_file()?;
//! let nca = Nca::from_file(nca)?;
//! let section = nca.section(0);
//! ```
//!
//! [switchbrew page]: https://switchbrew.org/w/index.php?title=NCA_Format

use crate::error::Error;
use crate::format::nca::structures::{ContentType, CryptoType, KeyType, RawNca, RawSuperblock};
use crate::impl_debug_deserialize_serialize_hexstring;
use crate::pki::{Aes128Key, AesXtsKey, Keys};
use binrw::BinRead;
use serde_derive::{Deserialize, Serialize};
use snafu::{Backtrace, GenerateImplicitData};
use std::cmp::max;
use std::io::Read;

mod structures;

#[repr(transparent)]
#[derive(Clone, Copy)]
struct Hash([u8; 0x20]);
impl_debug_deserialize_serialize_hexstring!(Hash);

#[derive(Debug, Serialize, Deserialize, Clone)]
enum FsType {
    Pfs0 {
        master_hash: Hash,
        block_size: u32,
        hash_table_offset: u64,
        hash_table_size: u64,
        pfs0_offset: u64,
        pfs0_size: u64,
    },
    RomFs,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
enum NcaFormat {
    Nca3,
    Nca2,
    Nca0,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SectionJson {
    media_start_offset: u32,
    media_end_offset: u32,
    unknown1: u32,
    unknown2: u32,
    crypto: CryptoType,
    fstype: FsType,
    nounce: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[repr(transparent)]
pub struct TitleId(u64);

impl std::fmt::Debug for TitleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NcaJson {
    format: NcaFormat,
    sig: structures::SigDebug,
    npdm_sig: structures::SigDebug,
    is_gamecard: bool,
    content_type: ContentType,
    key_revision: u8,
    key_type: KeyType,
    nca_size: u64,
    title_id: TitleId,
    sdk_version: u32, // TODO: Better format
    xts_key: AesXtsKey,
    ctr_key: Aes128Key,
    rights_id: Option<[u8; 0x10]>,
    sections: [Option<SectionJson>; 4],
}

#[derive(Debug)]
pub struct Nca<R> {
    stream: R,
    json: NcaJson,
}

fn get_key_area_key(pki: &Keys, key_version: usize, key_type: KeyType) -> Result<Aes128Key, Error> {
    let key = match key_type {
        KeyType::Application => pki.key_area_key_application()[key_version],
        KeyType::Ocean => pki.key_area_key_ocean()[key_version],
        KeyType::System => pki.key_area_key_system()[key_version],
    };
    key.ok_or(Error::MissingKey {
        key_name: Box::leak(
            format!("key_area_key_application_{:02x}", key_version).into_boxed_str(),
        ),
        backtrace: Backtrace::generate(),
    })
}

// Crypto is stupid. First, we need to get the max of crypto_type and crypto_type2.
// Then, nintendo uses both 0 and 1 as master key 0, and then everything is shifted by one.
// So we sub by 1.
fn get_master_key_revision(crypto_type: u8, crypto_type2: u8) -> u8 {
    max(crypto_type2, crypto_type).saturating_sub(1)
}

fn decrypt_header(pki: &Keys, file: &mut dyn Read) -> Result<RawNca, Error> {
    // Decrypt header.
    let mut header = [0; 0xC00];
    let mut decrypted_header = [0; 0xC00];

    file.read_exact(&mut header)?;

    // TODO: Check if NCA is already decrypted

    let header_key = pki.header_key().as_ref().ok_or(Error::MissingKey {
        key_name: "header_key",
        backtrace: Backtrace::generate(),
    })?;
    decrypted_header[..0x400].copy_from_slice(&header[..0x400]);
    header_key.decrypt(&mut decrypted_header[..0x400], 0, 0x200)?;

    // skip 2 signature blocks
    let magic = &decrypted_header[0x200..][..4];
    match magic {
        b"NCA3" => {
            decrypted_header.copy_from_slice(&header);
            header_key.decrypt(&mut decrypted_header, 0, 0x200)?;
        }
        b"NCA2" => {
            todo!()
            // for (i, fsheader) in raw_nca.fs_headers.iter().enumerate() {
            //     let offset = 0x400 + i * 0x200;
            //     if &fsheader._0x148[..] != &[0; 0xB8][..] {
            //         decrypted_header[offset..offset + 0x200]
            //             .copy_from_slice(&header[offset..offset + 0x200]);
            //         header_key.decrypt(&mut decrypted_header[offset..offset + 0x200], 0, 0x200)?;
            //     } else {
            //         decrypted_header[offset..offset + 0x200].copy_from_slice(&[0; 0x200]);
            //     }
            // }
        }
        b"NCA0" => unimplemented!("NCA0 parsing is not implemented yet"),
        _ => {
            return Err(Error::NcaParse {
                key_name: "header_key",
                backtrace: Backtrace::generate(),
            })
        }
    }

    // println!("{}", pretty_hex::pretty_hex(&decrypted_header));

    let mut raw_nca = std::io::Cursor::new(decrypted_header);
    let raw_nca = RawNca::read_le(&mut raw_nca).expect("RawNca to be of the right size");
    Ok(raw_nca)
}

impl<R: Read> Nca<R> {
    pub fn from_file(pki: &Keys, mut file: R) -> Result<Nca<R>, Error> {
        let header = decrypt_header(pki, &mut file)?;
        let format = match &header.magic {
            b"NCA3" => NcaFormat::Nca3,
            b"NCA2" => NcaFormat::Nca2,
            b"NCA0" => NcaFormat::Nca0,
            _ => unreachable!(),
        };

        // TODO: NCA: Verify header with RSA2048 PSS
        // BODY: We want to make sure the NCAs have a valid signature before
        // BODY: decrypting. Maybe put it behind a flag that accepts invalidly
        // BODY: signed NCAs?

        let master_key_revision = get_master_key_revision(header.crypto_type, header.crypto_type2);

        // Handle Rights ID.
        let has_rights_id = header.rights_id != [0; 0x10];

        let key_area_key = get_key_area_key(pki, master_key_revision as _, header.key_type)?;

        let decrypted_keys = if !has_rights_id {
            // TODO: NCA0 => return
            (
                key_area_key.derive_xts_key(&header.encrypted_xts_key)?,
                key_area_key.derive_key(&header.encrypted_ctr_key)?,
            )
        } else {
            // TODO: Implement RightsID crypto.
            unimplemented!("Rights ID");
        };

        // Parse sections
        let mut sections = [None, None, None, None];
        for (idx, (section, fs)) in header
            .section_entries
            .iter()
            .zip(header.fs_headers.iter())
            .enumerate()
        {
            // Check if section is present
            if let Some(fs) = fs {
                if has_rights_id {
                    unimplemented!("Rights ID");
                } else {
                    assert_eq!(fs.version, 2, "Invalid NCA FS Header version");
                    unsafe {
                        sections[idx] = Some(SectionJson {
                            crypto: fs.crypt_type.into(),
                            fstype: match fs.superblock {
                                RawSuperblock::Pfs0(s) => FsType::Pfs0 {
                                    master_hash: Hash(s.master_hash),
                                    block_size: s.block_size,
                                    hash_table_offset: s.hash_table_offset,
                                    hash_table_size: s.hash_table_size,
                                    pfs0_offset: s.pfs0_offset,
                                    pfs0_size: s.pfs0_size,
                                },
                                // RawSuperblock::RomFs => FsType::RomFs,
                                _ => unreachable!(),
                            },
                            nounce: fs.section_ctr,
                            media_start_offset: section.media_start_offset,
                            media_end_offset: section.media_end_offset,
                            unknown1: section.unknown1,
                            unknown2: section.unknown2,
                        });
                    }
                }
            }
        }

        let nca = Nca {
            stream: file,
            json: NcaJson {
                format,
                sig: header.fixed_key_sig,
                npdm_sig: header.npdm_sig,
                is_gamecard: header.is_gamecard != 0,
                content_type: ContentType::from(header.content_type),
                key_revision: master_key_revision,
                key_type: KeyType::from(header.key_type),
                nca_size: header.nca_size,
                title_id: TitleId(header.title_id),
                // TODO: Store the SDK version in a more human readable format.
                sdk_version: header.sdk_version,
                xts_key: decrypted_keys.0,
                ctr_key: decrypted_keys.1,
                // TODO: Implement rights id.
                rights_id: None,
                sections: sections,
            },
        };

        Ok(nca)
    }
}