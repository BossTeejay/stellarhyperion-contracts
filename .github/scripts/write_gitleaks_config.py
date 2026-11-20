"""Retain default secret rules while recognizing two reviewed public account fixtures."""

import base64
import binascii
from pathlib import Path
import re
import sys

PUBLIC_ACCOUNT_FIXTURES = (
    "GAAZI4TCR3TY5OJHCTJC2A4QSY6CJWJH5IAJTGKIN2ER7LBNVKOCCWN7",
    "GA5ZYIIVYDX5GRF2BKEQD2Y47ZCPG3HX3PUUNPVQ5ZECB57MJG7RLVNR",
)
FIXTURE_PATH = "script/stellar/test/reconcile-axelar.test.mjs"


def verify_public_account(value):
    """Version 48 denotes a public Ed25519 account; signing seeds use version 144."""
    if not isinstance(value, str) or not re.fullmatch(r"G[A-Z2-7]{55}", value):
        raise ValueError("Reviewed fixture must be a canonical Stellar public account")
    decoded = base64.b32decode(value)
    if (len(decoded) != 35 or decoded[0] != 48
            or decoded[-2:] != binascii.crc_hqx(decoded[:-2], 0).to_bytes(2, "little")):
        raise ValueError("Reviewed public account fixture has an invalid version or checksum")


def configuration():
    for value in PUBLIC_ACCOUNT_FIXTURES:
        verify_public_account(value)
    # Extend this rule without replacing its regex, entropy threshold or default allowlists.
    # AND is essential: neither the path alone nor the public value in another file is exempt.
    values = "|".join(PUBLIC_ACCOUNT_FIXTURES)
    path = re.escape(FIXTURE_PATH)
    return f"""[extend]
useDefault = true

[[rules]]
id = "generic-api-key"
[[rules.allowlists]]
description = "Two checksum-verified public Stellar account fixtures, never signing seeds"
condition = "AND"
regexTarget = "secret"
regexes = ['''^({values})$''']
paths = ['''^{path}$''']
"""


if __name__ == "__main__":
    Path(sys.argv[1]).write_text(configuration())
