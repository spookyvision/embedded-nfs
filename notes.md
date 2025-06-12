# mio 1.0

With v1 Mio is able to bump its MSRV to 1.70, allowing us to implement I/O
safety traits (https://github.com/rust-lang/rust/issues/87074) and replace
`SocketAddr` with the version found in the standard library.

https://github.com/tokio-rs/mio/blob/master/CHANGELOG.md