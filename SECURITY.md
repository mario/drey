# Security policy

## Reporting a vulnerability

Email <mario@djanic.com>. Do not open a public issue.

Include what you did, what happened, and how bad you think it is. A proof of
concept helps but is not required to start the conversation.

No response-time promise. This is a side project maintained by one person, and
an SLA I cannot keep is worth less than an honest "I read everything and I get
to it when I can." In practice that is usually days. If a week goes by with
nothing from me, ping again; the mail probably got buried rather than ignored.

Coordinated disclosure, and credit in the release notes unless you would rather
not have it. If you have waited a reasonable time and want to publish, tell me
your date rather than asking permission. Sitting on a real bug indefinitely
because a maintainer went quiet helps nobody.

## What is in scope

drey brokers connections between LSP clients and a shared language server on one
machine, for one user. The interesting failure is **cross-client leakage**: two
clients sharing a server when the sharing rules say they must not, one client
seeing another's document contents, or a response routed to the wrong client. If
you can make that happen, that is a real bug and I want to hear about it.

Also in scope: the daemon socket's permissions and ownership, anything that lets
another user on the same machine attach to your daemon, and the shell wrappers
that `scripts/install.sh` writes onto your `PATH`.

## What is not

drey runs language servers, and a language server executes project
configuration. Opening a hostile repository can run code regardless of drey.
That is the language server's trust model and drey does not change it.

The daemon is not a network service, does not authenticate, and is not meant to
be exposed beyond the local user. If you make it listen on a network, you are
outside the design.

## Supported versions

Pre-1.0. Fixes land on `main` and in the next release. There are no backported
patch branches yet.
