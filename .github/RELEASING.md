# Release signing setup

Automatic updates are authenticated by the public key committed in `release-signing.pub`. The
release workflow creates a detached Minisign signature for every platform binary. The client
downloads the binary and its `.minisig` file, then verifies the signature before replacing itself.

## Repository setup

The public key is intentionally committed: it is not secret and must be embedded in the client.
Configure one GitHub Actions repository secret named `RELEASE_SIGNING_KEY` containing the complete
private key that corresponds to `release-signing.pub`.

The key pair is generated outside the repository on a trusted machine:

```bash
umask 077
minisign -G -W -p release-signing.pub -s release-signing.key
```

Store `release-signing.key` as the GitHub secret, keep an access-controlled offline backup, and
securely remove the working copy. Never commit or share the private key.

The release job refuses to publish when the secret key does not match the committed public key. It
publishes each binary alongside a matching `<binary>.minisig` file.

## Key rotation

Rotation requires a transition release. Existing clients must authenticate that release with the
old private key, while its binaries embed the new public key. After clients have moved to the
transition release, subsequent binaries can be signed by the new key. The workflow's key-match
guard must be deliberately adjusted for that one transition release.
