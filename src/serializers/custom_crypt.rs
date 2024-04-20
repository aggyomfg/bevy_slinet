//! A custom packet serializer capable of handling encryption and decryption.
//! Demonstrates usage with mutable serializers that can mutate their internal state.

use std::marker::PhantomData;

use crate::serializer::{DefaultSerializationError, MutableSerializer};
use serde::{Deserialize, Serialize};

/// Represents custom packets sent from the client, allowing different types of content.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum CustomCryptClientPacket {
    String(String),
}

/// Represents custom packets received by the server, allowing different types of content.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum CustomCryptServerPacket {
    String(String),
}

/// Defines a trait for cryptographic engines with methods for packet encryption and decryption.
pub trait CryptEngine<ReceivingPacket, SendingPacket>: Default {
    fn encrypt(&mut self, packet: SendingPacket) -> Result<Vec<u8>, DefaultSerializationError>;
    fn decrypt(&mut self, packet: &[u8]) -> Result<ReceivingPacket, DefaultSerializationError>;
}

/// A simple key pair structure used for XOR encryption operations.
#[derive(Debug, Default, Clone)]
pub struct ExampleKeyPair(u64, u64);

/// A cryptographic engine implementing XOR encryption, typically not secure but used for demonstration.
#[derive(Debug, Default, Clone)]
pub struct CustomCryptEngine {
    key_pair: ExampleKeyPair,
}

impl CustomCryptEngine {
    /// Encrypts data using XOR operation and rotates the key to simulate a stream cipher.
    fn xor_encrypt(&mut self, data: Vec<u8>) -> Vec<u8> {
        let mut key = self.key_pair.0;
        let encrypted: Vec<u8> = data
            .into_iter()
            .map(|byte| {
                let result = byte ^ (key as u8);
                key = key.wrapping_add(1);
                result
            })
            .collect();
        self.key_pair.0 = key;
        encrypted
    }

    /// Decrypts data using XOR operation, ensuring the key is rotated in the same manner as encryption.
    fn xor_decrypt(&mut self, data: Vec<u8>) -> Vec<u8> {
        let mut key = self.key_pair.1;
        let decrypted: Vec<u8> = data
            .into_iter()
            .map(|byte| {
                let result = byte ^ (key as u8);
                key = key.wrapping_add(1);
                result
            })
            .collect();
        self.key_pair.1 = key;
        decrypted
    }
}

/// Implements the encryption and decryption processes for specified packet types using the XOR method.
/// This implement server-side encryption
impl CryptEngine<CustomCryptClientPacket, CustomCryptServerPacket> for CustomCryptEngine {
    fn encrypt(
        &mut self,
        packet: CustomCryptServerPacket,
    ) -> Result<Vec<u8>, DefaultSerializationError> {
        let packet_data = bincode::serialize(&packet).unwrap();
        let encrypted_data = self.xor_encrypt(packet_data);
        Ok(encrypted_data)
    }

    fn decrypt(
        &mut self,
        packet: &[u8],
    ) -> Result<CustomCryptClientPacket, DefaultSerializationError> {
        let decrypted_data = self.xor_decrypt(packet.to_vec());
        let packet = bincode::deserialize(&decrypted_data).unwrap();
        Ok(packet)
    }
}
// This is the client-side encryption
impl CryptEngine<CustomCryptServerPacket, CustomCryptClientPacket> for CustomCryptEngine {
    fn encrypt(
        &mut self,
        packet: CustomCryptClientPacket,
    ) -> Result<Vec<u8>, DefaultSerializationError> {
        let packet_data = bincode::serialize(&packet).unwrap();
        let encrypted_data = self.xor_encrypt(packet_data);
        Ok(encrypted_data)
    }

    fn decrypt(
        &mut self,
        packet: &[u8],
    ) -> Result<CustomCryptServerPacket, DefaultSerializationError> {
        let decrypted_data = self.xor_decrypt(packet.to_vec());
        let packet = bincode::deserialize(&decrypted_data).unwrap();
        Ok(packet)
    }
}

/// A serializer that integrates encryption, using a cryptographic engine to ensure secure data transmission.
#[derive(Default, Clone)]
pub struct CustomCryptSerializer<C, ReceivingPacket, SendingPacket>
where
    C: Send + Sync + 'static + CryptEngine<ReceivingPacket, SendingPacket>,
{
    crypt_engine: C,
    _client: PhantomData<ReceivingPacket>,
    _server: PhantomData<SendingPacket>,
}
impl<
        C: Send + Sync + 'static + CryptEngine<ReceivingPacket, SendingPacket>,
        SendingPacket,
        ReceivingPacket,
    > CustomCryptSerializer<C, ReceivingPacket, SendingPacket>
where
    C: Send + Sync + 'static + CryptEngine<ReceivingPacket, SendingPacket>,
{
    pub fn new(crypt_engine: C) -> Self {
        Self {
            crypt_engine,
            _client: PhantomData,
            _server: PhantomData,
        }
    }
}
impl<ReceivingPacket, SendingPacket, C> MutableSerializer<ReceivingPacket, SendingPacket>
    for CustomCryptSerializer<C, ReceivingPacket, SendingPacket>
where
    C: Send + Sync + 'static + CryptEngine<ReceivingPacket, SendingPacket>,
    ReceivingPacket: Send + Sync + 'static,
    SendingPacket: Send + Sync + 'static,
{
    type Error = DefaultSerializationError;

    /// Serializes a packet into a byte vector.
    fn serialize(&mut self, packet: SendingPacket) -> Result<Vec<u8>, Self::Error> {
        Ok(self.crypt_engine.encrypt(packet).unwrap())
    }

    /// Deserializes a packet from a byte slice
    fn deserialize(&mut self, buffer: &[u8]) -> Result<ReceivingPacket, Self::Error> {
        match self.crypt_engine.decrypt(buffer) {
            Ok(encrypted) => Ok(encrypted),
            Err(e) => {
                log::error!("{}", e);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xor_encrypt_decrypt() {
        let mut engine = CustomCryptEngine::default();
        let data = vec![1, 2, 3, 4, 5];

        let encrypted = engine.xor_encrypt(data.clone());
        assert_ne!(
            encrypted, data,
            "Encrypted data should not be equal to original data"
        );

        let decrypted = engine.xor_decrypt(encrypted);
        assert_eq!(
            decrypted, data,
            "Decrypted data should be equal to original data"
        );
    }

    #[test]
    fn test_crypt_engine_encrypt_decrypt() {
        let mut engine = CustomCryptEngine::default();
        let client_packet = CustomCryptClientPacket::String("Hello, Server!".to_string());
        let server_packet = CustomCryptServerPacket::String("Hello, Client!".to_string());

        // Client packet encryption and decryption
        let encrypted = engine.encrypt(client_packet.clone()).unwrap();
        let decrypted: CustomCryptClientPacket = engine.decrypt(&encrypted).unwrap();
        assert_eq!(
            decrypted, client_packet,
            "Decrypted client packet should be equal to original packet"
        );

        // Server packet encryption and decryption
        let encrypted = engine.encrypt(server_packet.clone()).unwrap();
        let decrypted: CustomCryptServerPacket = engine.decrypt(&encrypted).unwrap();
        assert_eq!(
            decrypted, server_packet,
            "Decrypted server packet should be equal to original packet"
        );
    }
}
