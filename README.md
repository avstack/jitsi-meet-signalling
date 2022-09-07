# jitsi-meet-signalling

A work-in-progress implementation of Jitsi Meet's signalling protocol in Rust.

This code was extracted from [gst-meet](https://github.com/avstack/gst-meet), which will eventually be modified to use this library.

This library only performs signalling, and provides an `Agent` trait for the specific WebRTC implementation to implement.
