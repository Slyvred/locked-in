use crate::helpers;
use helpers::print_progress_bar;
use std::{
    collections::HashSet,
    io::{BufReader, BufWriter, Read, Seek, Write},
};
use zeroize::Zeroize;

use serde::{Deserialize, Serialize};

use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    AeadCore, Aes256Gcm, Key,
};

use argon2::{self, Config};
use rand::RngCore;

// const STORED_DATA_SIZE: usize = 12; // Size of the chunk nonce
const HEADER_SIZE: usize = 44; //56 // Size of our encrypted file header
const ENC_BUFFER_SIZE: usize = 8192; // It's in the name
const DEC_BUFFER_SIZE: usize = ENC_BUFFER_SIZE + 12 + 16; // 8192 + 12 (= chunk nonce) + 16 (16 = AES-256 block size)

#[derive(Serialize, Deserialize)]
struct Header {
    filename_salt: [u8; 16],
    data_salt: [u8; 16],
    filename_nonce: [u8; 12],
    // data_nonce: [u8; 12],
}

impl Header {
    fn new() -> Self {
        Self {
            filename_salt: [0u8; 16],
            data_salt: [0u8; 16],
            filename_nonce: [0u8; 12],
            // data_nonce: [0u8; 12],
        }
    }
}

fn encrypt(
    key: &Key<Aes256Gcm>,
    nonce: &[u8; 12],
    plaintext: &[u8],
) -> Result<Vec<u8>, aes_gcm::Error> {
    let cipher = Aes256Gcm::new(key);
    cipher.encrypt(nonce.into(), plaintext)
}

/// Wrapper function to encrypt a file
pub fn encrypt_file(path: &str, password_str: &mut str, delete: bool) -> Result<(), &'static str> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return Err("Failed to open file"),
    };

    // Encrypt filename and extension
    let mut filename = path.split('/').last().unwrap();

    // If the split doesn't work, it means we're on Windows
    // We split on backslashes instead
    if filename == path {
        filename = path.split('\\').last().unwrap();
    }

    // Contains our two pairs of salt and nonce for the filename and the file data
    let mut header = Header::new();

    header.filename_salt = gen_salt();
    let mut filename_key = gen_key_from_password(password_str, &header.filename_salt);

    header.filename_nonce = Aes256Gcm::generate_nonce(&mut OsRng).into();

    let encrypted_filename = match encrypt(
        &filename_key.into(),
        &header.filename_nonce,
        filename.as_bytes(),
    ) {
        Ok(ct) => ct,
        Err(_) => return Err("Filename encryption failed"),
    };

    // Make sure we wipe the key from memory
    filename_key.zeroize();

    // Convert encrypted filename to hex string
    let encrypted_filename = hex::encode(encrypted_filename);

    // Max filename length is 255 on Linux and 260 on Windows
    if encrypted_filename.len() > 255 {
        return Err(
            "Encrypted filename is too long, please choose a shorter name for the original file",
        );
    }

    let output_path = path.replace(filename, &encrypted_filename);

    println!("Encrypting {} to {}", path, output_path);

    let output_file = match std::fs::File::create(output_path) {
        Ok(file) => file,
        Err(_) => return Err("Failed to create output file"),
    };

    let mut reader = BufReader::new(file);
    let mut writer = BufWriter::new(output_file);
    let mut buf = [0u8; ENC_BUFFER_SIZE];
    let file_size = std::fs::metadata(path).unwrap().len();
    // Generate a random salt and derive a key from the password
    header.data_salt = gen_salt();
    let mut key = gen_key_from_password(password_str, &header.data_salt);

    // Serializing the header
    let header_bytes = match bincode::serialize(&header) {
        Ok(bytes) => bytes,
        Err(_) => return Err("Header serialization failed"),
    };

    // Writing it to our file
    writer.write_all(&header_bytes).unwrap();

    // Contains all of our nonces
    let mut nonces_set: HashSet<[u8; 12]> = HashSet::new();
    nonces_set.insert(header.filename_nonce);

    while let Ok(bytes_read) = reader.read(&mut buf) {
        if bytes_read == 0 {
            break;
        }

        // Nonce used for the chunk
        let mut chunk_nonce: [u8; 12] = Aes256Gcm::generate_nonce(&mut OsRng).into();

        // nonces_set.insert() returns false is the value is already present in the HashSet
        // It means this nonce was already used and absolutely CANNOT be reused
        // So we generate a new one until the condition is satisfied
        while !nonces_set.insert(chunk_nonce) {
            println!("\nNonce reuse detected, generating a new one...");
            chunk_nonce = Aes256Gcm::generate_nonce(&mut OsRng).into();
        }

        let ciphertext = match encrypt(&key.into(), &chunk_nonce, &buf[..bytes_read]) {
            Ok(ct) => ct,
            Err(_) => return Err("Chunk encryption failed"),
        };

        writer.write_all(&chunk_nonce).unwrap();
        writer.write_all(&ciphertext).unwrap();

        print_progress_bar(
            reader.stream_position().unwrap() as f64 / file_size as f64,
            path,
        );
    }

    // Wipe key and password from memory
    key.zeroize();
    password_str.zeroize();

    // Print a newline after the progress bar
    println!();

    if delete {
        match std::fs::remove_file(path) {
            Ok(_) => (),
            Err(_) => return Err("Failed to delete original file"),
        }
    }

    Ok(())
}

fn decrypt(
    key: &Key<Aes256Gcm>,
    nonce: &[u8; 12],
    ciphertext: &[u8],
) -> Result<Vec<u8>, aes_gcm::Error> {
    let cipher = Aes256Gcm::new(key);
    cipher.decrypt(nonce.into(), ciphertext)
}

/// Wrapper function to decrypt a file
pub fn decrypt_file(path: &str, password_str: &mut str, delete: bool) -> Result<(), &'static str> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return Err("Failed to open file"),
    };

    let mut encrypted_filename_str = path.split('/').last().unwrap();

    // If the split doesn't work, it means we're on Windows
    // We split on backslashes instead
    if encrypted_filename_str == path {
        encrypted_filename_str = path.split('\\').last().unwrap();
    }

    let encrypted_filename = match hex::decode(encrypted_filename_str) {
        Ok(hex) => hex,
        Err(_) => {
            return Err("Failed to decode filename hex, are you sure this file is encrypted?")
        }
    };

    // Recover our header
    let mut header_buf = [0u8; HEADER_SIZE];
    let mut reader = BufReader::new(file);

    // Read the header from the first 56 bytes of the file
    match reader.read_exact(&mut header_buf) {
        Ok(_) => (),
        Err(_) => return Err("Failed to read header"),
    }

    let header: Header = match bincode::deserialize(&header_buf) {
        Ok(header) => header,
        Err(_) => return Err("Failed to deserialize file header"),
    };

    let mut filename_key = gen_key_from_password(password_str, &header.filename_salt);

    let filename = match decrypt(
        &filename_key.into(),
        &header.filename_nonce,
        &encrypted_filename,
    ) {
        Ok(pt) => pt,
        Err(_) => return Err("Wrong password"),
    };

    // Wipe filename key from memory
    filename_key.zeroize();

    let filename = String::from_utf8(filename).unwrap();
    let output_path = path.replace(encrypted_filename_str, &filename);

    println!("Decrypting {} to {}", path, output_path);

    let output_file = match std::fs::File::create(output_path) {
        Ok(file) => file,
        Err(_) => return Err("Failed to create output file"),
    };

    let mut writer = BufWriter::new(output_file);
    let mut buf = [0u8; DEC_BUFFER_SIZE];

    let file_size = std::fs::metadata(path).unwrap().len() - HEADER_SIZE as u64; // -56 bytes for the header size
    let num_chunks = file_size / DEC_BUFFER_SIZE as u64; // Number of 8KB chunks
    let remaining_bytes = file_size % DEC_BUFFER_SIZE as u64; // Remaining bytes, that don't fit in a chunk

    // Derive the key from the password and the salt
    let mut key = gen_key_from_password(password_str, &header.data_salt);

    for _ in 0..num_chunks {
        reader.read_exact(&mut buf).unwrap();

        // Extract nonce and ciphertext
        let chunk_nonce: [u8; 12] = buf[..12].try_into().unwrap();
        let ciphertext: [u8; DEC_BUFFER_SIZE - 12] = buf[12..].try_into().unwrap();

        let plaintext = match decrypt(&key.into(), &chunk_nonce, &ciphertext) {
            Ok(pt) => pt,
            Err(_) => return Err("Chunk decryption failed"),
        };

        writer.write_all(&plaintext).unwrap();

        print_progress_bar(
            reader.stream_position().unwrap() as f64 / file_size as f64,
            path,
        );
    }

    // Process remaining bytes, if any
    if remaining_bytes > 0 {
        // Read the last chunk
        let mut last_buf = vec![0u8; remaining_bytes as usize];
        reader.read_exact(&mut last_buf).unwrap();

        // Extract the relevant data
        let last_nonce: [u8; 12] = last_buf[..12].try_into().unwrap();
        let ciphertext: Vec<u8> = last_buf[12..].into();

        let plaintext = match decrypt(&key.into(), &last_nonce, &ciphertext) {
            Ok(pt) => pt,
            Err(_) => return Err("Last chunk decryption failed"),
        };

        writer.write_all(&plaintext).unwrap();
    }

    // Wipe key and password from memory
    key.zeroize();
    password_str.zeroize();

    // Print a newline after the progress bar
    println!();

    if delete {
        match std::fs::remove_file(path) {
            Ok(_) => (),
            Err(_) => return Err("Failed to delete original file"),
        }
    }

    Ok(())
}

/// Generate a random IV using OsRng
fn gen_salt() -> [u8; 16] {
    let mut iv = [0u8; 16];
    match OsRng.try_fill_bytes(&mut iv) {
        Ok(_) => iv,
        Err(_) => panic!("Failed to generate IV"),
    }
}

/// Generate a 32-byte key from a password string and a random salt using Argon2id
fn gen_key_from_password(password: &str, salt: &[u8]) -> [u8; 32] {
    let key = argon2::hash_raw(password.as_bytes(), salt, &Config::rfc9106_low_mem())
        .expect("Key derivation failed");

    assert_eq!(key.len(), 32); // Ensure the key is 32 bytes long

    let mut key_array = [0u8; 32];
    key_array.copy_from_slice(&key); // Copies exactly 32 bytes into the array
    key_array
}
