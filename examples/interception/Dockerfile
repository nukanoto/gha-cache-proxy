FROM ghcr.io/actions/actions-runner:2.336.0@sha256:0cfdcc701ce933c6d243c6b0b2da767366dc9f2e99961d4c3754b0b78084cdda

USER root
COPY runner-entrypoint.sh /usr/local/bin/gha-cache-runner
RUN chmod 0755 /usr/local/bin/gha-cache-runner
USER runner

ENTRYPOINT ["/usr/local/bin/gha-cache-runner"]
