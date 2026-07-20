---
name: Bug report
about: Something went wrong between your editor and the language server
labels: bug
---

**What happened**

<!-- What you did, what you expected, what you got instead. -->

**Setup**

- drey version / commit:
- OS:
- Client(s) (editor or agent, and version):
- Language server and version:

**Logs**

<!-- RUST_LOG=drey=debug drey serve <server>, the part around the failure. -->

```
```

**If clients shared when they should not have** (or refused to share when they
should have), paste the workspace roots each client sent in its `initialize`.
That is the first thing anyone will ask for.
