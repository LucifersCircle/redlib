FROM alpine:3.19

# Install wget
RUN apk add --no-cache wget

# Download redlib binary from the provided URL
RUN wget -O /tmp/redlib https://github.com/LucifersCircle/redlib/raw/main/target/release/redlib \
    && chmod +x /tmp/redlib \
    && mv /tmp/redlib /usr/bin/redlib \
    && chown root:root /usr/bin/redlib

# Expose port 8080 (if your application listens on a different port, change it accordingly)
EXPOSE 8080

# Set the command to run Redlib
CMD ["redlib"]
