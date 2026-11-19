# Security policy

## Reporting a vulnerability

Please report security problems **privately**, through GitHub's
**Security → Report a vulnerability** form on this repository (a private security
advisory). Do not open a public issue for them.

Include what you can: the version (`mochi --version`, or the Updates section of the app's Settings), the
platform, a description of the problem, and a way to reproduce it. A crafted archive
that triggers it is the most useful thing you can send; attach it to the advisory.

You will get an acknowledgement, and the fix is released together with an advisory that
credits you unless you prefer otherwise.

## What is in scope

- Anything an archive can make MOCHI do: crashes, hangs or unbounded memory on crafted
  `.mochi`, ZIP, 7z, RAR or TAR files; writing outside the chosen folder; following
  links during extraction; overwriting files the user did not choose to replace.
- Integrity: MOCHI reporting content as verified when it is not, or accepting a forged
  history as valid without saying so.
- The Encrypted profile: anything that reveals sealed names, sizes or content without the
  passphrase beyond what [`docs/user/encrypted.md`](docs/user/encrypted.md) documents,
  and any misuse of nonces or keys.
- The desktop app: anything that lets archive content or a web page reach the file
  system, the shell or the network.
- The updater: installing an update whose signature does not verify.

## Known limits (not vulnerabilities)

These are documented behaviour, listed so they are not reported as bugs:

- A `.mochi` file is not a backup; damage beyond a recovery record's capacity loses data.
- An Encrypted archive shows its number of commits, the number and sizes of its stored
  objects, and (because equal content is stored once) whether appended content was
  already in it; see the encryption guide.
- Removing a passphrase from an archive is not revocation: old copies of the archive
  still open with it.
- The installers are not code-signed; updates are authenticated by the updater's own
  signature.
- Without an expected head or local history, `verify` cannot detect that an archive was
  replaced by an older copy, and says so (`UNKNOWN` freshness).

## Supported versions

Security fixes are made to the latest release.
