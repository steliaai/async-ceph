# Contributing

## Code Formatting and Linting Standards

- Code must be formatted with Nightly `rustfmt`, i.e. `cargo +nightly fmt`.
- Code must have zero clippy warnings. Ensure `cargo clippy --workspace --all-features` produces a clean output.
- Dependencies are linted using `cargo-deny`. It can be installed via `cargo install cargo-deny` and ran via `cargo deny check`.
- All source files, documentation, scripts, workflows, etc. must end with a newline and have no trailing whitespace.

## Testing Standards

### Loom Tests

`async-ceph` contains low-level concurrency code whose correctness is checked via Loom. These tests can be run as follows:

```shell
RUSTFLAGS="$RUSTFLAGS --cfg loom" cargo test --lib --all-features --profile loom
```

### Miri Tests

`async-ceph` naturally makes use of unsafe code, so unit tests should also be run Under Miri:

```shell
cargo +nightly miri test --target x86_64-unknown-linux-gnu --lib --all-features
```

### Integration Tests

A `mkosi` configuration is available to build a disk image with tools to setup a single node Ceph cluster and QEMU configuration to add an additional disk for testing purposes. Go to the `support/mini-ceph` directory and build the VM image:

```shell
mkosi build
```

Boot the VM image (depending on your qemu config you may need to run this with root privileges):

```shell
mkosi vm
```

Within the VM, bootstrap ceph:

```shell
./ceph-bootstrap.sh
```

This will bootstrap the cluster, create a OSD on the virtual disk and print the minimal Ceph configuration required to connect to the cluster. Copy it to `<repo>/ceph.conf` so that tests can read it.

If you want to do these steps manually, here are the script's contents:

```shell
vm_ip=$(ip -4 address show dev enp0s1 | sed -n -E 's/\s+inet ([0-9\.]+)\/.*$/\1/p')
cephadm bootstrap -c /etc/ceph/initial.conf --mon-ip $vm_ip
# note: this takes a while and doesn't output progress, it's not broken!
ceph orch daemon add osd fedora:/dev/nvme0n1
# Copy output of this back to <repo>/ceph.conf
# Don't forget to add a trailing newline, parsing will fail otherwise
cat /etc/ceph/ceph.conf /etc/ceph/ceph.client.admin.keyring
```

Run all tests, including integration tests:

```shell
cargo test --all-features
```

### AddressSanitizer / ThreadSanitizer

We recommend running the test suite against these LLVM-based sanitizers, especially for aforementioned integration tests (which cannot be run under Miri).

- ASAN:

    ```shell
    RUSTFLAGS="$RUSTFLAGS -Zsanitizer=address" \
    cargo +nightly test --target x86_64-unknown-linux-gnu \
    --all-features
    ```

- TSAN:

    ```shell
    RUSTFLAGS="$RUSTFLAGS -Zsanitizer=thread" \
    cargo +nightly test --target x86_64-unknown-linux-gnu \
    --all-features
    ```

## Documentation Standards

The crates in this project are thoroughly documented. You can build the documentation locally using the `support/build-docs.sh` script. Ensure that it produces no errors or warnings.

## Developer Certificate of Origin

All contributions (including pull requests) must agree to the Developer Certificate of Origin (DCO) version 1.1. This is exactly the same one created and used by the Linux kernel developers and posted on http://developercertificate.org/. This is a developer's certification that he or she has the right to submit the patch for inclusion into the project.

Please include a "Signed-off-by" tag in every patch to confirm that you agree to the DCO. This can be done by passing `--signoff` to `git commit`.

## Commit Signing

All commits submitted to this project must be cryptographically signed using GPG or SSH keys. If you're new to commit signing, there are different ways to set it up:

#### Signing commits with a GPG key

1. [Generate a GPG key](https://docs.github.com/en/authentication/managing-commit-signature-verification/generating-a-new-gpg-key)
2. [Add the GPG key to your GitHub account](https://docs.github.com/en/authentication/managing-commit-signature-verification/adding-a-gpg-key-to-your-github-account)
3. [Configure `git` to use your GPG key for commit signing](https://docs.github.com/en/authentication/managing-commit-signature-verification/telling-git-about-your-signing-key#telling-git-about-your-gpg-key)

#### Signing commits with an SSH key

1. [Generate an SSH key and add it to `ssh-agent`](https://docs.github.com/en/authentication/connecting-to-github-with-ssh/generating-a-new-ssh-key-and-adding-it-to-the-ssh-agent)
2. [Add the SSH key to your GitHub account](https://docs.github.com/en/authentication/connecting-to-github-with-ssh/adding-a-new-ssh-key-to-your-github-account)
3. [Configure `git` to use your SSH key for commit signing](https://docs.github.com/en/authentication/managing-commit-signature-verification/telling-git-about-your-signing-key#telling-git-about-your-ssh-key)

## Licensing

The following text must be included at the beginning of every new source file, unless it is code that was copied from another permissively licensed project. In the latter case, said code's license must be included instead.

```
Copyright (c) 2026 Stelia Ltd
This project is dual-licensed under Apache 2.0 and MIT terms.

SPDX-License-Identifier: MIT OR Apache-2.0
```
