//! Demonstrates a stateful [`MutableSerializer`] with a replaceable [`CryptEngine`].

use std::marker::PhantomData;

use crate::serializers::serializer::MutableSerializer;
use bevy::log;
use bitcode::{Decode, Encode};

/// Carries example application data from a client.
#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub enum CustomCryptClientPacket {
    /// Carries a text payload.
    String(String),
}

impl Default for CustomCryptClientPacket {
    fn default() -> Self {
        Self::String(String::new())
    }
}

/// Carries example application data from a server.
#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub enum CustomCryptServerPacket {
    /// Carries a text payload.
    String(String),
}

impl Default for CustomCryptServerPacket {
    fn default() -> Self {
        Self::String(String::new())
    }
}

/// Reports a payload the example codec cannot decode.
#[derive(Debug, thiserror::Error)]
#[error("SerializationFailed")]
pub struct CustomSerializationError;

/// Supplies the payload transformation used by [`CustomCryptSerializer`].
pub trait CryptEngine<ReceivingPacket, SendingPacket>: Default {
    /// Transforms an outgoing packet.
    ///
    /// # Errors
    /// Returns an error when the engine cannot encode or transform the packet.
    fn encrypt(&mut self, packet: SendingPacket) -> Result<Vec<u8>, CustomSerializationError>;
    /// Reconstructs a packet using the engine’s current receive state.
    ///
    /// # Errors
    /// Returns an error when the payload cannot be decoded.
    fn decrypt(&mut self, packet: &[u8]) -> Result<ReceivingPacket, CustomSerializationError>;
}

/// Tracks independent send and receive positions for the example XOR transform.
#[derive(Clone, Debug, Default)]
pub struct ExampleKeyPair {
    send: XorPosition,
    receive: XorPosition,
}

#[derive(Clone, Debug, Default)]
struct XorPosition(u8);

impl XorPosition {
    fn transform(&mut self, mut data: Vec<u8>) -> Vec<u8> {
        for byte in &mut data {
            *byte ^= self.0;
            self.0 = self.0.wrapping_add(1);
        }
        data
    }
}

/// Demonstrates a stateful XOR transform; it provides no cryptographic security.
///
/// Requires reliable, ordered packets: loss, duplication, reordering or malformed input
/// desynchronizes its state, so it must not be used with UDP.
#[derive(Clone, Default)]
pub struct CustomCryptEngine {
    key_pair: ExampleKeyPair,
}

impl CustomCryptEngine {
    fn xor_encrypt(&mut self, data: Vec<u8>) -> Vec<u8> {
        self.key_pair.send.transform(data)
    }

    fn xor_decrypt(&mut self, data: Vec<u8>) -> Vec<u8> {
        self.key_pair.receive.transform(data)
    }
}

impl CryptEngine<CustomCryptClientPacket, CustomCryptServerPacket> for CustomCryptEngine {
    fn encrypt(
        &mut self,
        packet: CustomCryptServerPacket,
    ) -> Result<Vec<u8>, CustomSerializationError> {
        Ok(self.xor_encrypt(bitcode::encode(&packet)))
    }

    fn decrypt(
        &mut self,
        packet: &[u8],
    ) -> Result<CustomCryptClientPacket, CustomSerializationError> {
        let decrypted_data = self.xor_decrypt(packet.to_vec());
        bitcode::decode(&decrypted_data).map_err(|_| CustomSerializationError)
    }
}
impl CryptEngine<CustomCryptServerPacket, CustomCryptClientPacket> for CustomCryptEngine {
    fn encrypt(
        &mut self,
        packet: CustomCryptClientPacket,
    ) -> Result<Vec<u8>, CustomSerializationError> {
        Ok(self.xor_encrypt(bitcode::encode(&packet)))
    }

    fn decrypt(
        &mut self,
        packet: &[u8],
    ) -> Result<CustomCryptServerPacket, CustomSerializationError> {
        let decrypted_data = self.xor_decrypt(packet.to_vec());
        bitcode::decode(&decrypted_data).map_err(|_| CustomSerializationError)
    }
}

/// Delegates packet transformation to a connection-local [`CryptEngine`].
#[derive(Clone, Default)]
pub struct CustomCryptSerializer<C, ReceivingPacket, SendingPacket>
where
    C: Send + Sync + 'static + CryptEngine<ReceivingPacket, SendingPacket>,
{
    crypt_engine: C,
    _client: PhantomData<ReceivingPacket>,
    _server: PhantomData<SendingPacket>,
}
impl<C, ReceivingPacket, SendingPacket> CustomCryptSerializer<C, ReceivingPacket, SendingPacket>
where
    C: Send + Sync + 'static + CryptEngine<ReceivingPacket, SendingPacket>,
{
    /// Takes ownership of the engine, including its current send and receive state.
    pub const fn new(crypt_engine: C) -> Self {
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
    type EncodeError = CustomSerializationError;
    type DecodeError = CustomSerializationError;

    fn serialize(&mut self, packet: SendingPacket) -> Result<Vec<u8>, Self::EncodeError> {
        self.crypt_engine.encrypt(packet)
    }

    fn deserialize(&mut self, buffer: &[u8]) -> Result<ReceivingPacket, Self::DecodeError> {
        self.crypt_engine
            .decrypt(buffer)
            .inspect_err(|error| log::error!("{error}"))
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

        let encrypted = engine.encrypt(client_packet.clone()).unwrap();
        let decrypted: CustomCryptClientPacket = engine.decrypt(&encrypted).unwrap();
        assert_eq!(
            decrypted, client_packet,
            "Decrypted client packet should be equal to original packet"
        );

        let encrypted = engine.encrypt(server_packet.clone()).unwrap();
        let decrypted: CustomCryptServerPacket = engine.decrypt(&encrypted).unwrap();
        assert_eq!(
            decrypted, server_packet,
            "Decrypted server packet should be equal to original packet"
        );
    }

    #[test]
    fn test_decrypt_garbage_returns_error() {
        let mut engine = CustomCryptEngine::default();
        let result: Result<CustomCryptClientPacket, _> = engine.decrypt(&[0xFF, 0xFF, 0xFF]);
        assert!(result.is_err());
    }
}
