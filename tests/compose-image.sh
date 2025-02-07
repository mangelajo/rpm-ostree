#!/bin/bash
set -euo pipefail

# Pin to branch for some reproducibility
BRANCH=f37

dn=$(cd "$(dirname "$0")" && pwd)
topsrcdir=$(cd "$dn/.." && pwd)
commondir=$(cd "$dn/common" && pwd)
export topsrcdir commondir

# shellcheck source=common/libtest-core.sh
. "${commondir}/libtest.sh"
# Work around buggy check for overlayfs on /, but we're not writing to that
unset OSTREE_NO_XATTRS
unset OSTREE_SYSROOT_DEBUG

set -x

if test -z "${COMPOSE_KEEP_CACHE:-}"; then
    rm -rf compose-baseimage-test
    mkdir compose-baseimage-test
fi
cd compose-baseimage-test
mkdir -p cache

# First, let's test a minimal manifest, using repos from the host
rm minimal-test -rf
mkdir minimal-test
cd minimal-test
cat > minimal.yaml << 'EOF'
boot-location: modules
releasever: 36
packages:
  - bash
  - rpm
  - coreutils
  - selinux-policy-targeted
  - kernel
  - ostree
repos:
  - fedora  # Intentially using frozen GA repo
EOF
cp /etc/yum.repos.d/*.repo .
rpm-ostree compose image --cachedir=../cache --touch-if-changed=changed.stamp --initialize minimal.yaml minimal.ociarchive
# TODO actually test this container image
cd ..
echo "ok minimal"

# Next, test the full Fedora Silverblue config
test -d workstation-ostree-config || git clone --depth=1 https://pagure.io/workstation-ostree-config --branch "${BRANCH}"

rpm-ostree compose image --cachedir=cache --touch-if-changed=changed.stamp --initialize workstation-ostree-config/fedora-silverblue.yaml fedora-silverblue.ociarchive
skopeo inspect oci-archive:fedora-silverblue.ociarchive
test -f changed.stamp
rm -f changed.stamp
rpm-ostree compose image --cachedir=cache --offline --touch-if-changed=changed.stamp workstation-ostree-config/fedora-silverblue.yaml fedora-silverblue.ociarchive | tee out.txt
test '!' -f changed.stamp
assert_file_has_content_literal out.txt 'No apparent changes since previous commit'

echo "ok compose baseimage"
