FROM ghcr.io/actions/actions-runner:2.338.2

USER root
COPY runner-entrypoint.sh /usr/local/bin/gha-cache-runner
RUN chmod 0755 /usr/local/bin/gha-cache-runner

ENTRYPOINT ["/usr/local/bin/gha-cache-runner"]
