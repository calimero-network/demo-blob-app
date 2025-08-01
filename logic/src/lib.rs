#![allow(
    clippy::len_without_is_empty,
    reason = "BTreeMap and Vec don't need is_empty for this app"
)]

use std::collections::BTreeMap;
use std::io::{Read, Write};

use calimero_sdk::borsh::{BorshDeserialize, BorshSerialize};
use calimero_sdk::serde::Serialize;
use calimero_sdk::{app, env};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;

/// Safe base58 encoding for blob IDs using our own buffer
fn encode_blob_id_base58(blob_id_bytes: &[u8; 32]) -> String {
    let mut buf = [0u8; 44];
    let len = bs58::encode(blob_id_bytes).onto(&mut buf[..]).unwrap();
    std::str::from_utf8(&buf[..len]).unwrap().to_owned()
}

/// Parse a base58 string into blob ID bytes
fn parse_blob_id_base58(blob_id_str: &str) -> Result<[u8; 32], String> {
    let mut buf = [0u8; 32];
    let _ = bs58::decode(blob_id_str)
        .onto(&mut buf[..])
        .map_err(|e| format!("Invalid base58 blob ID: {}", e))?;
    Ok(buf)
}

#[app::state(emits = Event)]
#[derive(Debug, BorshSerialize, BorshDeserialize)]
#[borsh(crate = "calimero_sdk::borsh")]
pub struct ChatApp {
    messages: Vec<Message>,
    message_count: u64,
    // Mapping: compressed_blob_id -> decompressed_blob_id (for caching)
    decompression_cache: BTreeMap<[u8; 32], [u8; 32]>,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, Serialize)]
#[borsh(crate = "calimero_sdk::borsh")]
#[serde(crate = "calimero_sdk::serde")]
pub struct Message {
    pub id: u64,
    pub sender: String,
    pub text: String,
    pub attachments: Vec<Attachment>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, Serialize)]
#[borsh(crate = "calimero_sdk::borsh")]
#[serde(crate = "calimero_sdk::serde")]
pub struct Attachment {
    pub original_name: String,
    #[serde(serialize_with = "serialize_blob_id_bytes")]
    pub original_blob_id: [u8; 32],
    pub original_size: u64,
    #[serde(serialize_with = "serialize_blob_id_bytes")]
    pub compressed_blob_id: [u8; 32],
    pub compressed_size: u64,
    pub content_type: Option<String>,
    pub compression_ratio: f64, // compressed_size / original_size
}

/// Safe serialization function for blob ID bytes that handles BufferTooSmall panics
fn serialize_blob_id_bytes<S>(blob_id_bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
where
    S: calimero_sdk::serde::Serializer,
{
    let safe_string = encode_blob_id_base58(blob_id_bytes);
    serializer.serialize_str(&safe_string)
}

#[app::event]
#[derive(Debug)]
pub enum Event {
    MessageSent {
        message_id: u64,
        sender: String,
        text: String,
        attachment_count: usize,
    },
    AttachmentCompressed {
        original_blob_id: [u8; 32],
        compressed_blob_id: [u8; 32],
        original_size: u64,
        compressed_size: u64,
        compression_ratio: f64,
    },
}

/// Load complete blob data into memory (uses chunked reading internally)
fn load_blob_full(blob_id_bytes: &[u8; 32]) -> Result<Option<Vec<u8>>, String> {
    app::log!("Loading blob (32 bytes)");

    let fd = env::blob_open(blob_id_bytes);

    if fd == 0 {
        app::log!("Blob not found (handle is 0)");
        return Ok(None);
    }

    let mut result = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut total_read = 0;

    loop {
        let bytes_read = env::blob_read(fd, &mut buffer);

        if bytes_read == 0 {
            break;
        }

        result.extend_from_slice(&buffer[..bytes_read as usize]);
        total_read += bytes_read;
    }

    app::log!("Loaded blob: {} bytes", total_read);

    let _ = env::blob_close(fd);

    Ok(Some(result))
}

/// Store blob data using chunked writing
fn store_blob_chunked(data: &[u8]) -> Result<[u8; 32], String> {
    app::log!("Creating blob for {} bytes", data.len());

    let fd = env::blob_create();

    if fd == 0 {
        return Err("Failed to create blob handle".to_owned());
    }

    let chunk_size = 8192;
    let mut total_written = 0;

    for chunk in data.chunks(chunk_size) {
        let bytes_written = env::blob_write(fd, chunk);

        if bytes_written == 0 {
            return Err("Failed to write blob data".to_owned());
        }

        if bytes_written != chunk.len() as u64 {
            return Err(format!(
                "Partial write: wrote {} of {} bytes",
                bytes_written,
                chunk.len()
            ));
        }

        total_written += bytes_written;
    }

    if total_written != data.len() as u64 {
        return Err(format!(
            "Failed to write complete blob data: wrote {} of {} bytes",
            total_written,
            data.len()
        ));
    }

    let blob_id_buf = env::blob_close(fd);

    // Check if we got a valid blob ID (not all zeros)
    if blob_id_buf == [0u8; 32] {
        return Err("blob_close returned all zeros - blob creation failed".to_owned());
    }

    app::log!("Created blob with {} bytes", data.len());

    Ok(blob_id_buf)
}

/// Stream compress a blob without loading it entirely into memory
/// Returns (compressed_blob_id, original_size, compressed_size) or None if blob not found
fn stream_compress_blob(blob_id_bytes: &[u8; 32]) -> Result<Option<([u8; 32], u64, u64)>, String> {
    // Open source blob for reading
    let read_fd = env::blob_open(blob_id_bytes);
    if read_fd == 0 {
        return Ok(None);
    }

    // // Create destination blob for compressed data
    // let write_fd = env::blob_create();
    // if write_fd == 0 {
    //     let _ = env::blob_close(read_fd);
    //     return Err("Failed to create compressed blob handle".to_owned());
    // }

    // True streaming compression: read chunk → write to encoder → encoder writes compressed to blob
    struct BlobWriter {
        fd: u64,
        total_written: u64,
    }

    impl Write for BlobWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let written = env::blob_write(self.fd, buf);
            self.total_written += written;
            Ok(written as usize)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(()) // blob_write is immediate
        }
    }

    // let blob_writer = BlobWriter {
    //     fd: write_fd,
    //     total_written: 0,
    // };
    // let mut encoder = GzEncoder::new(blob_writer, Compression::default());
    let mut read_buffer = [0u8; 8192];
    let mut original_size = 0u64;

    // Stream: read chunk → write to encoder → encoder automatically writes compressed to blob
    loop {
        let bytes_read = env::blob_read(read_fd, &mut read_buffer);
        if bytes_read == 0 {
            break;
        }

        original_size += bytes_read;

        // // Write chunk to compressor (it automatically writes compressed data to blob)
        // encoder
        //     .write_all(&read_buffer[..bytes_read as usize])
        //     .map_err(|e| {
        //         let _ = env::blob_close(read_fd);
        //         let _ = env::blob_close(write_fd);
        //         format!("Compression write error: {}", e)
        //     })?;
    }

    // let _ = env::blob_close(read_fd);

    // Finish compression (flushes remaining compressed data to blob)
    // let blob_writer = encoder.finish().map_err(|e| {
    //     let _ = env::blob_close(write_fd);
    //     format!("Compression finish error: {}", e)
    // })?;

    // let compressed_size = blob_writer.total_written;

    // Check if compression helped (> 10% savings)
    // let compression_ratio = compressed_size as f64 / original_size as f64;
    // if compression_ratio >= 0.9 {
        // Compression didn't help, clean up and return original
        // let _ = env::blob_close(write_fd);
        return Ok(Some((*blob_id_bytes, original_size, original_size)));
    // }

    // Close and get blob ID
    // let compressed_blob_id = env::blob_close(write_fd);

    // Ok(Some((compressed_blob_id, original_size, compressed_size)))
}

/// Decompress data (handles both gzip and uncompressed)
fn decompress_data(compressed: &[u8]) -> Result<Vec<u8>, String> {
    let mut decoder = GzDecoder::new(compressed);
    let mut decompressed = Vec::new();

    match decoder.read_to_end(&mut decompressed) {
        Ok(_) => {
            app::log!(
                "Successfully gzip decompressed {} bytes to {} bytes",
                compressed.len(),
                decompressed.len()
            );
            Ok(decompressed)
        }
        Err(_) => {
            app::log!(
                "Data was not gzip compressed, returning original {} bytes",
                compressed.len()
            );
            Ok(compressed.to_vec())
        }
    }
}

#[app::logic]
impl ChatApp {
    #[app::init]
    pub fn init() -> ChatApp {
        app::log!("Initializing ChatApp");

        ChatApp {
            messages: Vec::new(),
            message_count: 0,
            decompression_cache: BTreeMap::new(),
        }
    }

    /// Send a message with text and attachment blob IDs (from HTTP upload)
    pub fn send_message(
        &mut self,
        sender: String,
        text: String,
        attachment_blob_ids: Vec<String>,
        attachment_names: Vec<String>,
        attachment_sizes: Vec<u64>,
        attachment_content_types: Vec<Option<String>>,
    ) -> app::Result<u64> {
        app::log!(
            "Sending message from '{}' with {} attachments",
            sender,
            attachment_blob_ids.len()
        );

        if attachment_blob_ids.len() != attachment_names.len()
            || attachment_blob_ids.len() != attachment_sizes.len()
            || attachment_blob_ids.len() != attachment_content_types.len()
        {
            app::bail!("Attachment metadata length mismatch");
        }

        let mut attachments = Vec::new();

        // Process each attachment: read, compress, store
        for (i, blob_id_str) in attachment_blob_ids.iter().enumerate() {
            app::log!("Processing attachment {}: {}", i, blob_id_str);

            // Parse blob ID
            let blob_id_bytes = parse_blob_id_base58(blob_id_str)
                .map_err(|e| app::err!("Failed to parse blob ID '{}': {}", blob_id_str, e))?;

            // Stream compress the blob without loading into memory
            let (compressed_blob_id_bytes, original_size, compressed_size) =
                stream_compress_blob(&blob_id_bytes)
                    .map_err(|e| app::err!("Failed to compress blob '{}': {}", blob_id_str, e))?
                    .ok_or_else(|| app::err!("Blob not found: {}", blob_id_str))?;

            let compression_ratio = compressed_size as f64 / original_size as f64;

            app::log!(
                "Compressed {} bytes to {} bytes (ratio: {:.2})",
                original_size,
                compressed_size,
                compression_ratio
            );

            app::emit!(Event::AttachmentCompressed {
                original_blob_id: blob_id_bytes,
                compressed_blob_id: compressed_blob_id_bytes,
                original_size,
                compressed_size,
                compression_ratio,
            });

            // Announce both original and compressed blobs to the network for discovery
            let current_context = env::context_id();
            
            // Announce original blob to network
            if env::blob_announce_to_context(&blob_id_bytes, &current_context) {
                app::log!("Successfully announced original blob {} to network", blob_id_str);
            } else {
                app::log!("Failed to announce original blob {} to network", blob_id_str);
            }
            
            // Announce compressed blob to network (if different from original)
            if compressed_blob_id_bytes != blob_id_bytes {
                let compressed_blob_str = encode_blob_id_base58(&compressed_blob_id_bytes);
                if env::blob_announce_to_context(&compressed_blob_id_bytes, &current_context) {
                    app::log!("Successfully announced compressed blob {} to network", compressed_blob_str);
                } else {
                    app::log!("Failed to announce compressed blob {} to network", compressed_blob_str);
                }
            }

            attachments.push(Attachment {
                original_name: attachment_names[i].clone(),
                original_blob_id: blob_id_bytes,
                original_size,
                compressed_blob_id: compressed_blob_id_bytes,
                compressed_size,
                content_type: attachment_content_types[i].clone(),
                compression_ratio,
            });
        }

        let message_id = self.message_count;
        self.message_count += 1;

        let attachment_count = attachments.len();

        let message = Message {
            id: message_id,
            sender: sender.clone(),
            text: text.clone(),
            attachments,
            timestamp: env::time_now(),
        };

        self.messages.push(message);

        app::emit!(Event::MessageSent {
            message_id,
            sender: sender,
            text: text,
            attachment_count,
        });

        app::log!("Message {} sent successfully", message_id);
        Ok(message_id)
    }

    /// Get all messages (without attachment data, just metadata)
    pub fn get_messages(&self) -> Vec<Message> {
        self.messages.clone()
    }

    /// Get a specific message by ID
    pub fn get_message(&self, message_id: u64) -> app::Result<Message> {
        self.messages
            .iter()
            .find(|m| m.id == message_id)
            .cloned()
            .ok_or_else(|| app::err!("Message not found: {}", message_id))
    }

    /// Get decompressed blob ID with lazy decompression and caching
    pub fn get_decompressed_blob_id(
        &mut self,
        compressed_blob_id_str: String,
    ) -> app::Result<String> {
        app::log!(
            "Getting decompressed blob ID for: {}",
            compressed_blob_id_str
        );

        let compressed_blob_id_bytes = parse_blob_id_base58(&compressed_blob_id_str)
            .map_err(|e| app::err!("Invalid compressed blob ID '{}': {}", compressed_blob_id_str, e))?;

        // Check if this blob was actually compressed by looking for it in recent messages
        // If compressed_blob_id == original_blob_id in any attachment, no decompression needed
        for message in &self.messages {
            for attachment in &message.attachments {
                if attachment.compressed_blob_id == compressed_blob_id_bytes {
                    if attachment.original_blob_id == compressed_blob_id_bytes {
                        app::log!("Blob was not compressed (same as original), returning as-is");
                        return Ok(compressed_blob_id_str);
                    }
                    break;
                }
            }
        }

        if let Some(decompressed_blob_id_bytes) =
            self.decompression_cache.get(&compressed_blob_id_bytes)
        {
            let fd = env::blob_open(decompressed_blob_id_bytes);
            if fd != 0 {
                let _ = env::blob_close(fd);
                app::log!("Cache hit: returning cached decompressed blob ID");
                return Ok(encode_blob_id_base58(&decompressed_blob_id_bytes));
            } else {
                app::log!("Cached decompressed blob no longer exists, removing from cache");
                let _ = self.decompression_cache.remove(&compressed_blob_id_bytes);
            }
        }

        app::log!("Cache miss: performing lazy decompression");

        // Load compressed data
        let compressed_data = load_blob_full(&compressed_blob_id_bytes)
            .map_err(|err| app::err!("Failed to load compressed blob: {}", err))?
            .ok_or_else(|| app::err!("Compressed blob not found: {}", compressed_blob_id_str))?;

        app::log!("Loaded compressed data: {} bytes", compressed_data.len());

        // Decompress the data
        let decompressed_data = decompress_data(&compressed_data)
            .map_err(|err| app::err!("Failed to decompress data: {}", err))?;

        app::log!(
            "Decompressed {} bytes to {} bytes",
            compressed_data.len(),
            decompressed_data.len()
        );

        // Store decompressed data as new blob (chunk by chunk)
        let decompressed_blob_id_bytes = store_blob_chunked(&decompressed_data)
            .map_err(|err| app::err!("Failed to store decompressed data: {}", err))?;

        // Cache the mapping
        let _ = self
            .decompression_cache
            .insert(compressed_blob_id_bytes, decompressed_blob_id_bytes);

        app::log!("Successfully decompressed and cached blob");

        Ok(encode_blob_id_base58(&decompressed_blob_id_bytes))
    }

    /// Get chat statistics
    pub fn get_stats(&self) -> app::Result<BTreeMap<String, u64>> {
        let mut stats = BTreeMap::new();

        let _ = stats.insert("total_messages".to_owned(), self.messages.len() as u64);

        let total_attachments: usize = self.messages.iter().map(|m| m.attachments.len()).sum();
        let _ = stats.insert("total_attachments".to_owned(), total_attachments as u64);

        let total_original_size: u64 = self
            .messages
            .iter()
            .flat_map(|m| &m.attachments)
            .map(|a| a.original_size)
            .sum();
        let _ = stats.insert("total_original_size_bytes".to_owned(), total_original_size);

        let total_compressed_size: u64 = self
            .messages
            .iter()
            .flat_map(|m| &m.attachments)
            .map(|a| a.compressed_size)
            .sum();
        let _ = stats.insert(
            "total_compressed_size_bytes".to_owned(),
            total_compressed_size,
        );

        let compression_savings = if total_original_size > 0 {
            if total_compressed_size <= total_original_size {
                ((total_original_size - total_compressed_size) as f64 * 100.0
                    / total_original_size as f64) as u64
            } else {
                0
            }
        } else {
            0
        };
        let _ = stats.insert(
            "compression_savings_percent".to_owned(),
            compression_savings,
        );

        // Add compression efficiency metric (values > 100 mean expansion, < 100 mean compression)
        let compression_efficiency = if total_original_size > 0 {
            (total_compressed_size as f64 * 100.0 / total_original_size as f64) as u64
        } else {
            100
        };
        let _ = stats.insert(
            "compression_efficiency_percent".to_owned(),
            compression_efficiency,
        );

        Ok(stats)
    }

    /// Clear all messages (for testing)
    pub fn clear_messages(&mut self) -> app::Result<()> {
        let count = self.messages.len();
        self.messages.clear();
        app::log!("Cleared {} messages", count);
        Ok(())
    }

    /// Test blob announcement functionality
    /// Creates a test blob, announces it to the network, and reports success
    pub fn test_blob_announcement(&mut self, test_data: String) -> app::Result<String> {
        app::log!("Testing blob announcement with data: '{}'", test_data);
        
        // Create a blob with the test data
        let test_bytes = test_data.as_bytes();
        let blob_id_bytes = store_blob_chunked(test_bytes)
            .map_err(|e| app::err!("Failed to create test blob: {}", e))?;
        
        let blob_id_str = encode_blob_id_base58(&blob_id_bytes);
        app::log!("Created test blob with ID: {}", blob_id_str);
        
        // Announce the blob to the current context
        let current_context = env::context_id();
        if env::blob_announce_to_context(&blob_id_bytes, &current_context) {
            app::log!("Successfully announced test blob {} to network", blob_id_str);
            Ok(format!("SUCCESS: Test blob {} announced to network", blob_id_str))
        } else {
            app::log!("Failed to announce test blob {} to network", blob_id_str);
            app::bail!("Failed to announce test blob to network")
        }
    }

    /// Test blob retrieval functionality  
    /// Attempts to read a blob by its ID (for testing discovery)
    pub fn test_blob_retrieval(&self, blob_id_str: String) -> app::Result<String> {
        app::log!("Testing blob retrieval for ID: {}", blob_id_str);
        
        let blob_id_bytes = parse_blob_id_base58(&blob_id_str)
            .map_err(|e| app::err!("Invalid blob ID '{}': {}", blob_id_str, e))?;
        
        // Try to load the blob
        match load_blob_full(&blob_id_bytes) {
            Ok(Some(data)) => {
                let content = String::from_utf8_lossy(&data);
                app::log!("Successfully retrieved blob {}: {} bytes", blob_id_str, data.len());
                Ok(format!("SUCCESS: Retrieved {} bytes - '{}'", data.len(), content))
            }
            Ok(None) => {
                app::log!("Blob {} not found", blob_id_str);
                app::bail!("Blob not found: {}", blob_id_str)
            }
            Err(e) => {
                app::log!("Error retrieving blob {}: {}", blob_id_str, e);
                app::bail!("Error retrieving blob: {}", e)
            }
        }
    }
}
