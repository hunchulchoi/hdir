# OpenSSH server for the real ssh tests (see scripts/ssh-docker-test.sh).
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends openssh-server python3 \
 && rm -rf /var/lib/apt/lists/* \
 && useradd -m -s /bin/sh hdir \
 && usermod -p '*' hdir \
 && mkdir -p /run/sshd
COPY entrypoint.sh /entrypoint.sh
CMD ["/bin/sh", "/entrypoint.sh"]
