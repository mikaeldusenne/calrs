# PodSaN downstream branch

The `podsan` branch carries the small set of changes required by the PodSaN deployment while `main` stays aligned with upstream Cal.rs.

## Downstream changes

- trust the CA bundle referenced by `SSL_CERT_FILE` for OIDC;
- allow SMTP configuration without AUTH and use native CA roots;
- authenticate resource ICS feeds with the resource's encrypted CalDAV credentials;
- use the PodSaN calendar icon;
- default newly created OIDC users and accounts to `Europe/Paris`;
- build with the PodSaN certificate image.

## Image build

When this branch is mirrored to GitLab, `.gitlab-ci.yml` builds two image tags in the Podman store shared with the trusted `podman-deploy` runner:

- `localhost/${EDS_IMAGE_PREFIX}-calrs:podsan-<short commit SHA>`: traceable build;
- `localhost/${EDS_IMAGE_PREFIX}-calrs:podsan`: moving deployment tag.

No container registry is required. The deployment project consumes the moving local tag directly and should verify that it exists before starting.

This design assumes that both pipelines use runners connected to the same Podman socket. Build the fork image again after storage cleanup or before deploying from another host.

## Sync diagnostics

For slow calendar syncs or dashboard HTTP 504s, see [sync diagnostics](docs/podsan-sync-diagnostics.md).
The observer and its tests live in `src/sync_diagnostics.rs` and `src/sync_diagnostics/`; keep upstream changes limited to the documented observation points.

## Updating from upstream

1. Fast-forward `main` from `olivierlambert/calrs`.
2. Rebase `podsan` onto the updated `main`.
3. Review every downstream commit and run the GitLab image build.
4. Validate OIDC, SMTP, user calendars, shared resources, booking, cancellation and rescheduling before deployment.
