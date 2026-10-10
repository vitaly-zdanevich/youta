# URL Info TLS test fixtures

These are a self-signed test CA and its RSA server certificate/private key, generated
solely for deterministic, in-memory TLS tests. They contain no production credentials.
The leaf covers `example.test`, `*.example.test`, and `other.test`, expires on
2126-09-16, and is trusted only by test-only client configuration. Tests use a fixed
2027 validation time. Production uses the normal WebPKI trust store.

The fixture was generated with OpenSSL `req -x509`, `req`, and `x509 -req`, using
`basicConstraints=critical,CA:FALSE`, `keyUsage=critical,digitalSignature,keyEncipherment`,
`extendedKeyUsage=serverAuth`, and the DNS subject alternative names above for the leaf.

See the [rustls connection metadata documentation](https://docs.rs/rustls/latest/rustls/struct.CommonState.html)
for the verified certificate chain and negotiated protocol exposed by the client.
