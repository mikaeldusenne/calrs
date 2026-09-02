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

When this branch is mirrored to GitLab, `.gitlab-ci.yml` publishes two image tags:

- `podsan-<short commit SHA>`: immutable traceable build;
- `podsan`: moving deployment tag.

The GitLab project hosting this mirror must use the trusted `podman-deploy` runner and enable its Container Registry.

## Updating from upstream

1. Fast-forward `main` from `olivierlambert/calrs`.
2. Rebase `podsan` onto the updated `main`.
3. Review every downstream commit and run the GitLab image build.
4. Validate OIDC, SMTP, user calendars, shared resources, booking, cancellation and rescheduling before deployment.
