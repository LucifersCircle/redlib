FROM ubuntu:22.04

# Download redlib binary from the provided URL
RUN apt-get update && apt-get install -y wget && \
    wget -O /usr/bin/redlib https://github.com/LucifersCircle/redlib/raw/main/target/release/redlib && \
    chmod +x /usr/bin/redlib && \
    apt-get purge -y wget && apt-get autoremove -y && rm -rf /var/lib/apt/lists/*

# Expose port 8080 (if your application listens on a different port, change it accordingly)
EXPOSE 8080

# Set the command to run Redlib
CMD ["redlib"]
