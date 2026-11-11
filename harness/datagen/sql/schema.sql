-- Provisional PostgreSQL schema for the datagen CSV output.
--
-- One table per CSV file; every table's column list matches that file's
-- header row exactly (checked by harness/datagen/tests/test_schema.py
-- against the table schemas in datagen/tables.py). Money and rates are
-- numeric(12,2) because the generator writes fixed-scale 2dp decimal
-- strings; quantities numeric(14,3) for the milli-unit 3dp strings;
-- dates ISO 8601 in a plain date column. All tables are provisional and
-- may be replaced wholesale by real storage schema work later.

CREATE TABLE companies (
    company_id      text        NOT NULL,
    name            text        NOT NULL,
    currency        text        NOT NULL,
    CONSTRAINT companies_pk PRIMARY KEY (company_id)
);

CREATE TABLE accounts (
    account_id      text        NOT NULL,
    code            text        NOT NULL,
    name            text        NOT NULL,
    kind            text        NOT NULL,
    CONSTRAINT accounts_pk PRIMARY KEY (account_id)
);

CREATE TABLE warehouses (
    warehouse_id    text        NOT NULL,
    company_id      text        NOT NULL,
    name            text        NOT NULL,
    CONSTRAINT warehouses_pk PRIMARY KEY (warehouse_id),
    CONSTRAINT warehouses_company_fk FOREIGN KEY (company_id) REFERENCES companies (company_id)
);

CREATE TABLE items (
    item_id         text        NOT NULL,
    code            text        NOT NULL,
    name            text        NOT NULL,
    unit            text        NOT NULL,
    standard_rate   numeric(12,2) NOT NULL,
    CONSTRAINT items_pk PRIMARY KEY (item_id)
);

CREATE TABLE customers (
    customer_id     text        NOT NULL,
    name            text        NOT NULL,
    CONSTRAINT customers_pk PRIMARY KEY (customer_id)
);

CREATE TABLE suppliers (
    supplier_id     text        NOT NULL,
    name            text        NOT NULL,
    CONSTRAINT suppliers_pk PRIMARY KEY (supplier_id)
);

CREATE TABLE opening_stock (
    opening_stock_id text       NOT NULL,
    company_id      text        NOT NULL,
    warehouse_id    text        NOT NULL,
    item_id         text        NOT NULL,
    date            date        NOT NULL,
    quantity        numeric(14,3) NOT NULL,
    valuation_rate  numeric(12,2) NOT NULL,
    amount          numeric(12,2) NOT NULL,
    CONSTRAINT opening_stock_pk PRIMARY KEY (opening_stock_id),
    CONSTRAINT opening_stock_company_fk FOREIGN KEY (company_id) REFERENCES companies (company_id),
    CONSTRAINT opening_stock_warehouse_fk FOREIGN KEY (warehouse_id) REFERENCES warehouses (warehouse_id),
    CONSTRAINT opening_stock_item_fk FOREIGN KEY (item_id) REFERENCES items (item_id)
);

CREATE TABLE opening_invoice (
    invoice_id      text        NOT NULL,
    company_id      text        NOT NULL,
    direction       text        NOT NULL,
    party_type      text        NOT NULL,
    party_id        text        NOT NULL,
    date            date        NOT NULL,
    is_opening      boolean     NOT NULL,
    total           numeric(12,2) NOT NULL,
    CONSTRAINT opening_invoice_pk PRIMARY KEY (invoice_id),
    CONSTRAINT opening_invoice_company_fk FOREIGN KEY (company_id) REFERENCES companies (company_id)
);

CREATE TABLE opening_balance (
    opening_balance_id text     NOT NULL,
    company_id      text        NOT NULL,
    account_id      text        NOT NULL,
    date            date        NOT NULL,
    amount          numeric(12,2) NOT NULL,
    CONSTRAINT opening_balance_pk PRIMARY KEY (opening_balance_id),
    CONSTRAINT opening_balance_company_fk FOREIGN KEY (company_id) REFERENCES companies (company_id),
    CONSTRAINT opening_balance_account_fk FOREIGN KEY (account_id) REFERENCES accounts (account_id)
);

CREATE TABLE journal_entry (
    entry_id        text        NOT NULL,
    company_id      text        NOT NULL,
    date            date        NOT NULL,
    memo            text        NOT NULL,
    CONSTRAINT journal_entry_pk PRIMARY KEY (entry_id),
    CONSTRAINT journal_entry_company_fk FOREIGN KEY (company_id) REFERENCES companies (company_id)
);

CREATE TABLE journal_entry_line (
    journal_entry_id text       NOT NULL,
    line            integer     NOT NULL,
    account_id      text        NOT NULL,
    debit           numeric(12,2) NOT NULL,
    credit          numeric(12,2) NOT NULL,
    CONSTRAINT journal_entry_line_pk PRIMARY KEY (journal_entry_id, line),
    CONSTRAINT journal_entry_line_entry_fk FOREIGN KEY (journal_entry_id) REFERENCES journal_entry (entry_id),
    CONSTRAINT journal_entry_line_account_fk FOREIGN KEY (account_id) REFERENCES accounts (account_id)
);

CREATE TABLE stock_transfer (
    transfer_id     text        NOT NULL,
    company_id      text        NOT NULL,
    date            date        NOT NULL,
    from_warehouse_id text      NOT NULL,
    to_warehouse_id text        NOT NULL,
    item_id         text        NOT NULL,
    quantity        numeric(14,3) NOT NULL,
    rate            numeric(12,2) NOT NULL,
    amount          numeric(12,2) NOT NULL,
    CONSTRAINT stock_transfer_pk PRIMARY KEY (transfer_id),
    CONSTRAINT stock_transfer_company_fk FOREIGN KEY (company_id) REFERENCES companies (company_id),
    CONSTRAINT stock_transfer_from_fk FOREIGN KEY (from_warehouse_id) REFERENCES warehouses (warehouse_id),
    CONSTRAINT stock_transfer_to_fk FOREIGN KEY (to_warehouse_id) REFERENCES warehouses (warehouse_id),
    CONSTRAINT stock_transfer_item_fk FOREIGN KEY (item_id) REFERENCES items (item_id)
);

CREATE TABLE stock_adjustment (
    adjustment_id   text        NOT NULL,
    company_id      text        NOT NULL,
    warehouse_id    text        NOT NULL,
    item_id         text        NOT NULL,
    date            date        NOT NULL,
    direction       text        NOT NULL,
    reason_code     text        NOT NULL,
    quantity        numeric(14,3) NOT NULL,
    rate            numeric(12,2) NOT NULL,
    amount          numeric(12,2) NOT NULL,
    CONSTRAINT stock_adjustment_pk PRIMARY KEY (adjustment_id),
    CONSTRAINT stock_adjustment_company_fk FOREIGN KEY (company_id) REFERENCES companies (company_id),
    CONSTRAINT stock_adjustment_warehouse_fk FOREIGN KEY (warehouse_id) REFERENCES warehouses (warehouse_id),
    CONSTRAINT stock_adjustment_item_fk FOREIGN KEY (item_id) REFERENCES items (item_id)
);

CREATE TABLE sales_invoice (
    invoice_id      text        NOT NULL,
    company_id      text        NOT NULL,
    warehouse_id    text        NOT NULL,
    customer_id     text        NOT NULL,
    date            date        NOT NULL,
    total           numeric(12,2) NOT NULL,
    CONSTRAINT sales_invoice_pk PRIMARY KEY (invoice_id),
    CONSTRAINT sales_invoice_company_fk FOREIGN KEY (company_id) REFERENCES companies (company_id),
    CONSTRAINT sales_invoice_warehouse_fk FOREIGN KEY (warehouse_id) REFERENCES warehouses (warehouse_id),
    CONSTRAINT sales_invoice_customer_fk FOREIGN KEY (customer_id) REFERENCES customers (customer_id)
);

CREATE TABLE sales_invoice_line (
    sales_invoice_id text       NOT NULL,
    line            integer     NOT NULL,
    item_id         text        NOT NULL,
    quantity        numeric(14,3) NOT NULL,
    rate            numeric(12,2) NOT NULL,
    amount          numeric(12,2) NOT NULL,
    CONSTRAINT sales_invoice_line_pk PRIMARY KEY (sales_invoice_id, line),
    CONSTRAINT sales_invoice_line_invoice_fk FOREIGN KEY (sales_invoice_id) REFERENCES sales_invoice (invoice_id),
    CONSTRAINT sales_invoice_line_item_fk FOREIGN KEY (item_id) REFERENCES items (item_id)
);

CREATE TABLE purchase_invoice (
    invoice_id      text        NOT NULL,
    company_id      text        NOT NULL,
    warehouse_id    text        NOT NULL,
    supplier_id     text        NOT NULL,
    date            date        NOT NULL,
    total           numeric(12,2) NOT NULL,
    is_replenishment boolean    NOT NULL,
    CONSTRAINT purchase_invoice_pk PRIMARY KEY (invoice_id),
    CONSTRAINT purchase_invoice_company_fk FOREIGN KEY (company_id) REFERENCES companies (company_id),
    CONSTRAINT purchase_invoice_warehouse_fk FOREIGN KEY (warehouse_id) REFERENCES warehouses (warehouse_id),
    CONSTRAINT purchase_invoice_supplier_fk FOREIGN KEY (supplier_id) REFERENCES suppliers (supplier_id)
);

CREATE TABLE purchase_invoice_line (
    purchase_invoice_id text    NOT NULL,
    line            integer     NOT NULL,
    item_id         text        NOT NULL,
    quantity        numeric(14,3) NOT NULL,
    rate            numeric(12,2) NOT NULL,
    amount          numeric(12,2) NOT NULL,
    CONSTRAINT purchase_invoice_line_pk PRIMARY KEY (purchase_invoice_id, line),
    CONSTRAINT purchase_invoice_line_invoice_fk FOREIGN KEY (purchase_invoice_id) REFERENCES purchase_invoice (invoice_id),
    CONSTRAINT purchase_invoice_line_item_fk FOREIGN KEY (item_id) REFERENCES items (item_id)
);

CREATE TABLE payment (
    payment_id      text        NOT NULL,
    direction       text        NOT NULL,
    party_type      text        NOT NULL,
    party_id        text        NOT NULL,
    date            date        NOT NULL,
    amount          numeric(12,2) NOT NULL,
    CONSTRAINT payment_pk PRIMARY KEY (payment_id)
);

CREATE TABLE payment_allocation (
    payment_id      text        NOT NULL,
    line            integer     NOT NULL,
    invoice_id      text        NOT NULL,
    amount          numeric(12,2) NOT NULL,
    CONSTRAINT payment_allocation_pk PRIMARY KEY (payment_id, line),
    CONSTRAINT payment_allocation_payment_fk FOREIGN KEY (payment_id) REFERENCES payment (payment_id)
);

CREATE TABLE sales_return (
    sales_return_id text        NOT NULL,
    company_id      text        NOT NULL,
    warehouse_id    text        NOT NULL,
    customer_id     text        NOT NULL,
    original_invoice_id text    NOT NULL,
    date            date        NOT NULL,
    total           numeric(12,2) NOT NULL,
    CONSTRAINT sales_return_pk PRIMARY KEY (sales_return_id),
    CONSTRAINT sales_return_company_fk FOREIGN KEY (company_id) REFERENCES companies (company_id),
    CONSTRAINT sales_return_warehouse_fk FOREIGN KEY (warehouse_id) REFERENCES warehouses (warehouse_id),
    CONSTRAINT sales_return_customer_fk FOREIGN KEY (customer_id) REFERENCES customers (customer_id),
    CONSTRAINT sales_return_original_fk FOREIGN KEY (original_invoice_id) REFERENCES sales_invoice (invoice_id)
);

CREATE TABLE sales_return_line (
    sales_return_id text        NOT NULL,
    line            integer     NOT NULL,
    item_id         text        NOT NULL,
    quantity        numeric(14,3) NOT NULL,
    rate            numeric(12,2) NOT NULL,
    amount          numeric(12,2) NOT NULL,
    original_line   integer     NOT NULL,
    CONSTRAINT sales_return_line_pk PRIMARY KEY (sales_return_id, line),
    CONSTRAINT sales_return_line_return_fk FOREIGN KEY (sales_return_id) REFERENCES sales_return (sales_return_id),
    CONSTRAINT sales_return_line_item_fk FOREIGN KEY (item_id) REFERENCES items (item_id)
);

CREATE TABLE purchase_return (
    purchase_return_id text     NOT NULL,
    company_id      text        NOT NULL,
    warehouse_id    text        NOT NULL,
    supplier_id     text        NOT NULL,
    original_invoice_id text    NOT NULL,
    date            date        NOT NULL,
    total           numeric(12,2) NOT NULL,
    CONSTRAINT purchase_return_pk PRIMARY KEY (purchase_return_id),
    CONSTRAINT purchase_return_company_fk FOREIGN KEY (company_id) REFERENCES companies (company_id),
    CONSTRAINT purchase_return_warehouse_fk FOREIGN KEY (warehouse_id) REFERENCES warehouses (warehouse_id),
    CONSTRAINT purchase_return_supplier_fk FOREIGN KEY (supplier_id) REFERENCES suppliers (supplier_id),
    CONSTRAINT purchase_return_original_fk FOREIGN KEY (original_invoice_id) REFERENCES purchase_invoice (invoice_id)
);

CREATE TABLE purchase_return_line (
    purchase_return_id text     NOT NULL,
    line            integer     NOT NULL,
    item_id         text        NOT NULL,
    quantity        numeric(14,3) NOT NULL,
    rate            numeric(12,2) NOT NULL,
    amount          numeric(12,2) NOT NULL,
    original_line   integer     NOT NULL,
    CONSTRAINT purchase_return_line_pk PRIMARY KEY (purchase_return_id, line),
    CONSTRAINT purchase_return_line_return_fk FOREIGN KEY (purchase_return_id) REFERENCES purchase_return (purchase_return_id),
    CONSTRAINT purchase_return_line_item_fk FOREIGN KEY (item_id) REFERENCES items (item_id)
);