# Synthetic TLS rejection fixture

`untrusted-localhost.der` is a self-signed RSA certificate for localhost/127.0.0.1.
`untrusted-localhost-key.der` is its PKCS#8 test-only private key. Both were generated
locally for negative TLS tests; they are not credentials for any service or game.
Never use this public fixture key for a deployment. The test expects both proxy and
origin TLS validation to reject this certificate before HTTP application data.

Generated with OpenSSL `req -x509 -newkey rsa:2048 -nodes`, subject CN=localhost,
SAN DNS:localhost,IP:127.0.0.1; converted using `x509 -outform DER` and
`pkcs8 -topk8 -nocrypt -outform DER`. No trust-store changes are required.
