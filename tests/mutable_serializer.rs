use bevy_slinet::serializer::{MutableSerializer, Serializer, SerializerAdapter};
use std::{
    cell::Cell,
    convert::Infallible,
    sync::{Arc, Mutex},
    thread,
};

// Cell models a third-party codec that is Send but not Sync.
struct StatefulBytes(Cell<usize>);

impl MutableSerializer<Vec<u8>, Vec<u8>> for StatefulBytes {
    type EncodeError = Infallible;
    type DecodeError = Infallible;

    fn serialize(&mut self, packet: Vec<u8>) -> Result<Vec<u8>, Infallible> {
        self.0.set(self.0.get() + 1);
        Ok(packet)
    }

    fn deserialize(&mut self, buffer: &[u8]) -> Result<Vec<u8>, Infallible> {
        self.0.set(self.0.get() + 1);
        Ok(buffer.to_vec())
    }
}

#[test]
fn send_only_codec_can_be_shared_through_the_adapter() {
    let codec = Arc::new(Mutex::new(StatefulBytes(Cell::new(0))));
    let adapter = SerializerAdapter::Mutable(codec.clone());

    thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                assert_eq!(adapter.serialize(vec![1, 2, 3]), Ok(vec![1, 2, 3]));
                assert_eq!(adapter.deserialize(&[4, 5, 6]), Ok(vec![4, 5, 6]));
            });
        }
    });

    assert!(matches!(codec.lock(), Ok(codec) if codec.0.get() == 8));
}
