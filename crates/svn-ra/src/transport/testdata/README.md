Self-signed test certificates for `tls_server_end_point` (RFC 5929) with signature
algorithms `rcgen` cannot produce. Generated once with OpenSSL 3.0:

```sh
openssl dsaparam -out dsap.pem 2048 && openssl gendsa -out dsa.key dsap.pem
openssl req -x509 -new -key dsa.key -sha256 -days 36500 -subj /CN=svn.example.com -outform DER -out dsa-sha256.der
openssl ecparam -name prime256v1 -genkey -noout -out ec.key
openssl req -x509 -new -key ec.key -sha224 -days 36500 -subj /CN=svn.example.com -outform DER -out ecdsa-sha224.der
openssl genrsa -out rsa.key 2048
openssl req -x509 -new -key rsa.key -sha224 -days 36500 -subj /CN=svn.example.com -outform DER -out rsa-sha224.der
```

The private keys were discarded; the certificates are only hashed, never trusted.
