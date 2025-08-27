#!/bin/sh

# Find the line specifying the version number for the Rust toolchain, then
# extract that version number. Failure to do so aborts the CI run.
msrv=$(
    awk '/^rust-version = "[[:digit:]]+\.[[:digit:]]+\.[[:digit:]]+"$/ {
      print substr($NF, 2, length($NF) - 2)
    }' Cargo.toml
)
if [ -z $msrv ]; then
    printf "Could not determine Rust toolchain version\n"
    exit 1
fi

# Check for one of two conditions to regenerate the Docker image:
# (These conditions must be met on `main` and not a PR branch, see ci.yml)
# 1. An annotated tag was found when fetching the tags at depth 1 (tags on HEAD)
# 2. "$UBUNTU_DOCKERFILE" was changed between HEAD and the prior commit (HEAD~1)
git fetch --depth=1 origin +refs/tags/*:refs/tags/*
if ! git diff HEAD~1 --quiet -- "$UBUNTU_DOCKERFILE" ||
    git describe --candidates=0; then
    should_build=true
else
    should_build=false
fi

echo "msrv=$msrv" >> "$GITHUB_OUTPUT"
echo "should_build=$should_build" >> "$GITHUB_OUTPUT"
echo "container_path=$REGISTRY/$1" >> "$GITHUB_OUTPUT"
