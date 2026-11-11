# Security policy

Spotledger Runtime is **pre-release software** (version 0.x). It has had no independent security audit. Do not rely on it to protect data you cannot afford to lose or expose. Known weaknesses are tracked while the code is in development.

## Supported versions

There are no supported releases yet. Security fixes land on the default branch (`main`) of this repository. Once there are tagged releases, this section will say which ones receive fixes.

## Reporting a vulnerability

Please report suspected vulnerabilities **privately**. Do not open a public issue or discussion for a security problem.

- Use GitHub's private vulnerability reporting: open the **Security** tab of this repository and choose **Report a vulnerability**.

Please include what you found, how to reproduce it, the version or commit, and what you think the impact is. A proof of concept helps. Do not include real personal data.

## What to expect

- We will acknowledge a report within **7 days** and aim to tell you whether we consider it a vulnerability within **30 days**.
- We will keep you informed about the fix and credit you in the advisory if you want that.
- We ask for **coordinated disclosure**: please give us a reasonable time to fix a problem before you publish details. We will agree a date with you; 90 days is our default.

## Scope

In scope: code in this repository, and the contracts it publishes (storage, events, native API, app API, UI extension).

Out of scope: denial of service by volume alone, findings that need a configuration that the documentation tells you not to use, social engineering, and third-party dependencies (report those to their maintainers; tell us if our use of them makes the problem worse).

## Good-faith research

We will not take legal action against research carried out in good faith under this policy: it stays within scope, avoids other people's data, does not degrade other people's systems, and is reported privately.
