# Synthetic Master vector

These files are generated test data, not extracted game content.

- Key: bytes `00..1f`; IV: bytes `20..3f` (32 bytes each).
- Clear text: `master-synthetic.json`.
- Layout: 32 zero bytes, Rijndael-256-CBC(PKCS7(32 zero bytes + gzip(JSON))).
- A separate Python Rijndael implementation generated and independently decoded
  this ciphertext before the vector was saved.
- SHA-256 of `master-synthetic.bin`: `2ab094c0a35d383567fc5582498aae73593d341b0ccb304826d07850c0498def`.

No downloaded game content is included.

# Proxy protocol baseline

`proxy-descriptors.pb` is an independently retained descriptor subset for the ten
RPCs listed in `src/routes.rs`, their transitive message/enum dependencies, and the
`skip_authentication` annotation. It contains no additional game methods or types.
It is compared with the checked-in proto sources and native codecs in tests and
is never loaded by the server. Do not replace it with a full game descriptor set.
