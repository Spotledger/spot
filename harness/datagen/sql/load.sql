-- Load a generated CSV dataset into the provisional schema.
--
-- Run from the dataset directory (the one holding manifest.json and the
-- .csv files), after creating the schema:
--
--   createdb "$SPOT_DB"
--   psql -X -v ON_ERROR_STOP=1 -f <repo>/harness/datagen/sql/schema.sql "$SPOT_DB"
--   psql -X -v ON_ERROR_STOP=1 -f <repo>/harness/datagen/sql/load.sql "$SPOT_DB"
--
-- The \copy order below matches the schema's foreign-key dependency order:
-- master data first, then opening rows, then transaction tables, parent
-- rows before their line rows. Every load is all-or-nothing: wrap the whole
-- file in one transaction with --single-transaction if you prefer.

\copy companies        FROM 'companies.csv'        WITH (FORMAT csv, HEADER true)
\copy accounts         FROM 'accounts.csv'         WITH (FORMAT csv, HEADER true)
\copy warehouses       FROM 'warehouses.csv'       WITH (FORMAT csv, HEADER true)
\copy items            FROM 'items.csv'            WITH (FORMAT csv, HEADER true)
\copy customers        FROM 'customers.csv'        WITH (FORMAT csv, HEADER true)
\copy suppliers        FROM 'suppliers.csv'        WITH (FORMAT csv, HEADER true)

\copy opening_stock    FROM 'opening_stock.csv'    WITH (FORMAT csv, HEADER true)
\copy opening_invoice  FROM 'opening_invoice.csv'  WITH (FORMAT csv, HEADER true)
\copy opening_balance  FROM 'opening_balance.csv'  WITH (FORMAT csv, HEADER true)

\copy journal_entry    FROM 'journal_entry.csv'    WITH (FORMAT csv, HEADER true)
\copy journal_entry_line FROM 'journal_entry_line.csv' WITH (FORMAT csv, HEADER true)

\copy stock_transfer   FROM 'stock_transfer.csv'   WITH (FORMAT csv, HEADER true)
\copy stock_adjustment FROM 'stock_adjustment.csv' WITH (FORMAT csv, HEADER true)

\copy sales_invoice    FROM 'sales_invoice.csv'    WITH (FORMAT csv, HEADER true)
\copy sales_invoice_line FROM 'sales_invoice_line.csv' WITH (FORMAT csv, HEADER true)

\copy purchase_invoice FROM 'purchase_invoice.csv' WITH (FORMAT csv, HEADER true)
\copy purchase_invoice_line FROM 'purchase_invoice_line.csv' WITH (FORMAT csv, HEADER true)

\copy payment          FROM 'payment.csv'          WITH (FORMAT csv, HEADER true)
\copy payment_allocation FROM 'payment_allocation.csv' WITH (FORMAT csv, HEADER true)

\copy sales_return     FROM 'sales_return.csv'     WITH (FORMAT csv, HEADER true)
\copy sales_return_line FROM 'sales_return_line.csv' WITH (FORMAT csv, HEADER true)

\copy purchase_return  FROM 'purchase_return.csv'  WITH (FORMAT csv, HEADER true)
\copy purchase_return_line FROM 'purchase_return_line.csv' WITH (FORMAT csv, HEADER true)