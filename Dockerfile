FROM alpine:3.19

# Copy your compiled Rust binary into the container
COPY target/release/redlib /usr/local/bin/

# Add any additional dependencies if needed
RUN apk add --no-cache curl

# Create a non-root user to run the application
RUN adduser --home /nonexistent --no-create-home --disabled-password redlib
USER redlib

# Tell Docker to expose port 8080 (if your application listens on a different port, change it accordingly)
EXPOSE 8080

# Run a healthcheck every minute to make sure the application is functional
HEALTHCHECK --interval=1m --timeout=3s CMD wget --spider --q http://localhost:8080/settings || exit 1

# Set the command to run when the container starts
CMD ["redlib"]
