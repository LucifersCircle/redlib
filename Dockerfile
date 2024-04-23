FROM alpine:3.19

# Install wget
RUN apk add --no-cache wget

# Download redlib binary from the provided URL
RUN wget https://github.com/LucifersCircle/redlib/raw/main/target/release/redlib -O /tmp/redlib

# Move the binary to /usr/bin
RUN mv /tmp/redlib /usr/bin/redlib

# Make the binary executable
RUN chmod +x /usr/bin/redlib

# Expose port 8080 (if your application listens on a different port, change it accordingly)
EXPOSE 8080

# Set the command to run Redlib
CMD ["redlib"]
