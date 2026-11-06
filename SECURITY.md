# Security policy

## Supported versions

Security fixes go into the latest stable release. Upgrade using the installer or the binary
archives on [GitHub Releases](https://github.com/majiidd/vayu/releases/latest).

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's **Report a vulnerability** button on
this repository's Security tab, not in a public issue. Include:

- the affected version;
- how to reproduce the problem;
- what an attacker gains.

You can expect an acknowledgement within a week. A fix or mitigation plan follows once the
report is confirmed.

Examples of problems in scope:

- TLS or certificate verification failures;
- credentials, cookies, or signed URLs leaking to other hosts or into output;
- path traversal from server-supplied file names;
- a resumed file accepted with data that does not belong to it;
- memory-safety problems.

## Verifying downloads

Every release carries a `SHA256SUMS` file and a build provenance attestation for each archive.
The attestation is signed through Sigstore by the release workflow.

```sh
sha256sum -c SHA256SUMS --ignore-missing          # shasum -a 256 -c on macOS
gh attestation verify vayu-aarch64-apple-darwin.tar.gz --repo majiidd/vayu
```

The installer script checks `SHA256SUMS` before it installs anything.

## Privacy

Vayu has no telemetry, crash reporting, or update checks. It connects only to:

- the URLs you give it and the redirects they lead to;
- the proxies you configure, or that your system settings name.
