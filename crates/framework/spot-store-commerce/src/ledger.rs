//! The revenue ledger: append-only orders, fees and payouts, and a finance export.
//!
//! Nothing here posts to the accounts; the ERP's accounting is a separate concern. This is
//! the store's own record of what was sold, what the provider kept and what the publisher
//! is owed, kept **append-only**: a duplicate order or event id is refused rather than
//! overwritten, so the ledger is a faithful history.

use std::sync::Mutex;

use serde_json::Value;
use spot_money::{Currency, CurrencyTable, Money};

use crate::catalogue::Price;
use crate::error::{Result, StoreError, codes};

/// What kind of order a ledger row records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderKind {
    /// The first payment for a listing.
    Purchase,
    /// A subscription renewal.
    Renewal,
    /// A refund (a negative amount is not used; the kind says it is a reversal).
    Refund,
}

/// One order: a payment or a refund.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Order {
    /// The store's order id.
    pub order_id: String,
    /// The tenant that paid.
    pub tenant: String,
    /// The listing bought from.
    pub listing_id: String,
    /// The app bought.
    pub app: String,
    /// The publisher to pay.
    pub publisher: String,
    /// The plan.
    pub plan: String,
    /// The amount, in integer minor units.
    pub amount: Price,
    /// Opaque tax/VAT lines from the provider; never interpreted here.
    pub tax_lines: Option<Value>,
    /// The provider reference the order is about.
    pub provider_ref: String,
    /// The provider event id that produced the order (the idempotency key).
    pub event_id: String,
    /// What kind of order this is.
    pub kind: OrderKind,
    /// When the order was recorded (UTC seconds).
    pub at: i64,
}

/// Who keeps a fee.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeeKind {
    /// The payment provider's processing fee.
    Provider,
    /// The store's platform fee.
    Platform,
}

/// One fee charged against an order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fee {
    /// The store's fee id.
    pub fee_id: String,
    /// The order it is charged against.
    pub order_id: String,
    /// Who keeps it.
    pub kind: FeeKind,
    /// The amount, in integer minor units.
    pub amount: Price,
    /// When the fee was recorded (UTC seconds).
    pub at: i64,
}

/// One payout to a publisher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payout {
    /// The store's payout id.
    pub payout_ref: String,
    /// The publisher paid.
    pub publisher: String,
    /// The amount, in integer minor units.
    pub amount: Price,
    /// The first instant the payout covers (UTC seconds).
    pub period_start: i64,
    /// The last instant the payout covers (UTC seconds).
    pub period_end: i64,
    /// When the payout was recorded (UTC seconds).
    pub at: i64,
}

/// The net totals of one currency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrencyNet {
    /// The currency.
    pub currency: Currency,
    /// Gross order minor units.
    pub gross_minor: i64,
    /// Fee minor units.
    pub fee_minor: i64,
    /// Payout minor units.
    pub payout_minor: i64,
    /// Gross minus fees minus payouts.
    pub net_minor: i64,
}

/// The finance export: the whole ledger plus the per-currency net.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinanceExport {
    /// Every order, in recording order.
    pub orders: Vec<Order>,
    /// Every fee, in recording order.
    pub fees: Vec<Fee>,
    /// Every payout, in recording order.
    pub payouts: Vec<Payout>,
    /// The per-currency net, in currency order.
    pub net: Vec<CurrencyNet>,
}

/// The append-only revenue ledger.
#[derive(Debug, Default)]
pub struct Ledger {
    orders: Mutex<Vec<Order>>,
    fees: Mutex<Vec<Fee>>,
    payouts: Mutex<Vec<Payout>>,
}

impl Ledger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends an order.
    ///
    /// # Errors
    /// [`codes::LEDGER_ERROR`] when the order id or the provider event id is already
    /// recorded: the ledger never overwrites history.
    pub fn record_order(&self, order: Order) -> Result<Order> {
        let mut orders = self
            .orders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if orders
            .iter()
            .any(|row| row.order_id == order.order_id || row.event_id == order.event_id)
        {
            return Err(StoreError::new(
                codes::LEDGER_ERROR,
                "the order or its provider event is already recorded",
            ));
        }
        orders.push(order.clone());
        Ok(order)
    }

    /// Appends a fee.
    ///
    /// # Errors
    /// [`codes::LEDGER_ERROR`] when the fee id is already recorded or its order is
    /// unknown.
    pub fn record_fee(&self, fee: Fee) -> Result<Fee> {
        if !self
            .orders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|row| row.order_id == fee.order_id)
        {
            return Err(StoreError::new(
                codes::LEDGER_ERROR,
                "the fee names an unknown order",
            ));
        }
        let mut fees = self
            .fees
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if fees.iter().any(|row| row.fee_id == fee.fee_id) {
            return Err(StoreError::new(
                codes::LEDGER_ERROR,
                "the fee is already recorded",
            ));
        }
        fees.push(fee.clone());
        Ok(fee)
    }

    /// Appends a payout.
    ///
    /// # Errors
    /// [`codes::LEDGER_ERROR`] when the payout id is already recorded.
    pub fn record_payout(&self, payout: Payout) -> Result<Payout> {
        let mut payouts = self
            .payouts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if payouts
            .iter()
            .any(|row| row.payout_ref == payout.payout_ref)
        {
            return Err(StoreError::new(
                codes::LEDGER_ERROR,
                "the payout is already recorded",
            ));
        }
        payouts.push(payout.clone());
        Ok(payout)
    }

    /// The order a provider event produced, if any.
    #[must_use]
    pub fn order_by_event(&self, event_id: &str) -> Option<Order> {
        self.orders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|row| row.event_id == event_id)
            .cloned()
    }

    /// Every order, in recording order.
    #[must_use]
    pub fn orders(&self) -> Vec<Order> {
        self.orders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Every fee, in recording order.
    #[must_use]
    pub fn fees(&self) -> Vec<Fee> {
        self.fees
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Every payout, in recording order.
    #[must_use]
    pub fn payouts(&self) -> Vec<Payout> {
        self.payouts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The orders for one publisher, in recording order.
    #[must_use]
    pub fn orders_for_publisher(&self, publisher: &str) -> Vec<Order> {
        self.orders()
            .into_iter()
            .filter(|order| order.publisher == publisher)
            .collect()
    }

    /// Exports the whole ledger and computes the per-currency net exactly with
    /// `spot-money` (no floats anywhere).
    ///
    /// # Errors
    /// [`codes::MONEY_ERROR`] when a currency is missing from the table or a sum overflows.
    pub fn export(&self, table: &CurrencyTable) -> Result<FinanceExport> {
        let orders = self.orders();
        let fees = self.fees();
        let payouts = self.payouts();

        let mut currencies: Vec<Currency> = Vec::new();
        for price in orders
            .iter()
            .map(|order| order.amount)
            .chain(fees.iter().map(|fee| fee.amount))
            .chain(payouts.iter().map(|payout| payout.amount))
        {
            if !currencies.contains(&price.currency()) {
                currencies.push(price.currency());
            }
        }
        currencies.sort();

        let mut net = Vec::with_capacity(currencies.len());
        for currency in currencies {
            let gross = sum_prices(
                orders
                    .iter()
                    .filter(|order| order.amount.currency() == currency)
                    .map(|order| &order.amount),
                currency,
                table,
            )?;
            let fee_total = sum_prices(
                fees.iter()
                    .filter(|fee| fee.amount.currency() == currency)
                    .map(|fee| &fee.amount),
                currency,
                table,
            )?;
            let payout_total = sum_prices(
                payouts
                    .iter()
                    .filter(|payout| payout.amount.currency() == currency)
                    .map(|payout| &payout.amount),
                currency,
                table,
            )?;
            net.push(CurrencyNet {
                currency,
                gross_minor: gross.minor_units(),
                fee_minor: fee_total.minor_units(),
                payout_minor: payout_total.minor_units(),
                net_minor: gross
                    .minor_units()
                    .saturating_sub(fee_total.minor_units())
                    .saturating_sub(payout_total.minor_units()),
            });
        }
        Ok(FinanceExport {
            orders,
            fees,
            payouts,
            net,
        })
    }
}

/// Sums prices of one currency exactly, returning minor units.
fn sum_prices<'a>(
    prices: impl IntoIterator<Item = &'a Price>,
    currency: Currency,
    table: &CurrencyTable,
) -> Result<Price> {
    let monies: Vec<Money> = prices
        .into_iter()
        .map(|price| price.money(table))
        .collect::<Result<Vec<_>>>()?;
    let total = Money::sum(&currency, monies.iter())?;
    Price::from_money(&total, table)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Fee, FeeKind, Ledger, Order, OrderKind, Payout};
    use crate::catalogue::Price;
    use crate::error::codes;
    use crate::test_support::{table, usd};

    fn order(order_id: &str, event_id: &str, minor: i64) -> Order {
        Order {
            order_id: order_id.to_owned(),
            tenant: "acme".to_owned(),
            listing_id: "list-1".to_owned(),
            app: "my_app".to_owned(),
            publisher: "acme".to_owned(),
            plan: "pro_monthly".to_owned(),
            amount: Price::new(minor, usd()).unwrap(),
            tax_lines: None,
            provider_ref: "fake-session-sess-1".to_owned(),
            event_id: event_id.to_owned(),
            kind: OrderKind::Purchase,
            at: 100,
        }
    }

    #[test]
    fn the_ledger_is_append_only() {
        let ledger = Ledger::new();
        ledger.record_order(order("order-1", "evt-1", 990)).unwrap();
        assert_eq!(
            ledger
                .record_order(order("order-1", "evt-2", 990))
                .unwrap_err()
                .code(),
            codes::LEDGER_ERROR
        );
        assert_eq!(
            ledger
                .record_order(order("order-2", "evt-1", 990))
                .unwrap_err()
                .code(),
            codes::LEDGER_ERROR
        );
        assert_eq!(ledger.orders().len(), 1);
    }

    #[test]
    fn the_export_nets_gross_fees_and_payouts() {
        let ledger = Ledger::new();
        ledger
            .record_order(order("order-1", "evt-1", 1_000))
            .unwrap();
        ledger
            .record_fee(Fee {
                fee_id: "fee-1".to_owned(),
                order_id: "order-1".to_owned(),
                kind: FeeKind::Provider,
                amount: Price::new(50, usd()).unwrap(),
                at: 101,
            })
            .unwrap();
        ledger
            .record_payout(Payout {
                payout_ref: "pay-1".to_owned(),
                publisher: "acme".to_owned(),
                amount: Price::new(800, usd()).unwrap(),
                period_start: 0,
                period_end: 1_000,
                at: 200,
            })
            .unwrap();
        let export = ledger.export(&table()).unwrap();
        assert_eq!(export.net.len(), 1);
        assert_eq!(export.net[0].gross_minor, 1_000);
        assert_eq!(export.net[0].fee_minor, 50);
        assert_eq!(export.net[0].payout_minor, 800);
        assert_eq!(export.net[0].net_minor, 150);
        assert_eq!(export.orders.len(), 1);
        assert_eq!(export.fees.len(), 1);
        assert_eq!(export.payouts.len(), 1);
    }

    #[test]
    fn a_fee_for_an_unknown_order_is_refused() {
        let ledger = Ledger::new();
        assert_eq!(
            ledger
                .record_fee(Fee {
                    fee_id: "fee-1".to_owned(),
                    order_id: "order-x".to_owned(),
                    kind: FeeKind::Platform,
                    amount: Price::new(10, usd()).unwrap(),
                    at: 101,
                })
                .unwrap_err()
                .code(),
            codes::LEDGER_ERROR
        );
    }
}
