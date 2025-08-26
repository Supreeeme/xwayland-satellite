FROM ubuntu:latest

ARG DEBIAN_FRONTEND=noninteractive
ENV TZ=Etc/UTC
RUN apt-get update \
	&& apt-get install -y xwayland libxcb1 clang libxcb-cursor0 libxcb-cursor-dev curl pkg-config libegl1 \
	&& rm -r /var/lib/apt/lists/*
RUN mkdir /run/xwls-test
ENV XDG_RUNTIME_DIR="/run/xwls-test"
