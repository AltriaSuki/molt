"""Plain-text rendering of invoices, for emails and the billing log."""

from .money import format_amount

DESCRIPTION_WIDTH = 28
RULE = "-" * 73


def _clip(text, width):
    """Shorten ``text`` to ``width`` characters, marking the cut with '...'."""
    return text if len(text) <= width else text[: width - 3] + "..."


def _total_row(label, amount):
    return f"{label:<57}{amount:>16}"


def render(invoice):
    """Render ``invoice`` as a fixed-width text block."""
    out = [
        f"INVOICE {invoice.number}",
        f"Customer: {invoice.customer}",
        f"Currency: {invoice.currency}",
        RULE,
        f"{'Description':<{DESCRIPTION_WIDTH}}{'Qty':>6}{'Unit price':>16}"
        f"{'Disc':>7}{'Amount':>16}",
    ]
    for item in invoice.items:
        disc = f"{item.discount_percent:g}%" if item.discount_percent else ""
        out.append(
            f"{_clip(item.description, DESCRIPTION_WIDTH):<{DESCRIPTION_WIDTH}}"
            f"{item.quantity:>6}{format_amount(item.unit_price):>16}"
            f"{disc:>7}{format_amount(item.total):>16}"
        )
    out.append(RULE)
    out.append(_total_row("Subtotal", f"{invoice.subtotal:,}"))
    for category, amount in invoice.tax_by_category().items():
        rate = invoice.tax_table.rate(category)
        out.append(_total_row(f"  {category} tax ({rate:g}%)", f"{amount:,}"))
    out.append(_total_row("Tax", f"{invoice.tax:,}"))
    out.append(_total_row("Total", f"{invoice.total:,}"))
    return "\n".join(out)


def summary(invoice):
    """One line for listings: number, customer, item count and total."""
    count = len(invoice.items)
    noun = "item" if count == 1 else "items"
    return (
        f"{invoice.number} for {invoice.customer}: {count} {noun}, "
        f"total {invoice.currency} {invoice.total:,}"
    )
