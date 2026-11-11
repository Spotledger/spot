"""Spotledger synthetic data generator.

Deterministic, stdlib-only generator for neutral business datasets used by
the scale tests (blueprint §3 L9) and the reporting work: a company,
warehouse, item and party master, a small provisional chart of accounts,
opening balances and opening stock, sales and purchase invoices with lines,
payments allocated against invoices, stock transfers and adjustments,
returns and journal entries. Byte-for-byte reproducible given a profile and
a seed; standard library only; no floats anywhere.
"""
GENERATOR_NAME = "spot-datagen"
GENERATOR_VERSION = "2.0.0"