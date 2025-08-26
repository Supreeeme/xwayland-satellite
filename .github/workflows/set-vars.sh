#!/bin/sh

msrv=$(
    awk '/^rust-version = "[[:digit:]]+\.[[:digit:]]+\.[[:digit:]]+"$/ {
      print substr($NF, 2, length($NF) - 2)
    }' Cargo.toml
)
if [ -z $msrv ]; then
    printf "Could not determine Rust toolchain version\n"
    exit 1
fi

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
