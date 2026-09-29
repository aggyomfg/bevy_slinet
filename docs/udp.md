# UDP

[README](../README.md) · [Example](../examples/hello_world_udp.rs)

One serialized packet is one datagram, with no library header or length prefix.
`LengthSerializer` is ignored. Packets may be lost, duplicated or reordered;
your serializer must decode each datagram independently.

Client establishment means local socket readiness and sends nothing. Send the
first application packet to make the server discover the peer. The server
preserves that datagram for delivery. Neither event authenticates the sender.

`disconnect()` only removes local state: no remote notification, heartbeat or
idle timeout is provided. A later datagram can recreate the peer. Applications
own session identity, authentication, retries and expiry; see the
[session example](../examples/udp_application_sessions.rs).

Configure limits through [UdpConfig / UdpOptions](../src/protocols/udp/settings.rs).
Include your application envelope in the payload budget. There is no automatic
fragmentation, reassembly or path-MTU discovery. Peer allocation happens before
application validation, so bound and expire unvalidated peers too.

[Transport handles](../src/protocols/udp/diagnostics.rs) expose counters and send-rate
controls. Sent counters mean local socket acceptance; pacing is not adaptive
congestion control.
