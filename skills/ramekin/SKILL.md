---
name: ramekin
description: Use when customizing the ramekin container image with a .ramekin/Dockerfile
---

# Ramekin container environment

## Customizing the image

Create `.ramekin/Dockerfile` in the workspace. It builds on top of
`ramekin-agent`:

```dockerfile
FROM ramekin-agent
RUN apt-get update && apt-get install -y --no-install-recommends \
    postgresql-client && rm -rf /var/lib/apt/lists/*
```

The image is rebuilt on every `ramekin run`.

---

*This is a self-improving skill — see the `self-improving-skills` skill.*
