"""Writes synthetic mail for checking Jolta's AI features: .eml files dated in
the last ten days, for one made-up person, Sam Peeters <synthetic@jolta.invalid>.
Every person and company in them is invented. Each file carries the expected
sorting in an X-Jolta-Expect header ("work yes": category, needs reply), which
`jolta ai sort --check` compares against the model.

    python3 -I scripts/ai-fixtures.py <out-dir>
"""

import datetime
import email.utils
import os
import sys
from email.message import EmailMessage

ME = "Sam Peeters <synthetic@jolta.invalid>"
NOW = datetime.datetime.now(datetime.timezone(datetime.timedelta(hours=1)))

# (days ago, hours, from, subject, body, expect, extra headers, thread key)
MAIL = [
    (9, 9, "Lies Vermeulen <lies@vermeulen-bouw.invalid>", "Planning keukenrenovatie",
     "Dag Sam,\n\nZoals besproken starten we met de afbraak van de oude keuken op maandag 16 november. "
     "De nieuwe kasten worden geleverd op 23 november. Kunnen jullie de keuken tegen dan leegmaken?\n\nGroeten,\nLies",
     "work yes", {}, "kitchen"),
    (8, 20, ME, "Re: Planning keukenrenovatie",
     "Dag Lies,\n\nDat lukt, de keuken is leeg tegen zondag 15 november. Moeten we zelf nog iets regelen voor de elektriciteit?\n\nSam",
     None, {}, "kitchen"),
    (7, 10, "Lies Vermeulen <lies@vermeulen-bouw.invalid>", "Re: Planning keukenrenovatie",
     "Dag Sam,\n\nVoor de elektriciteit komt onze elektricien Bart op 18 november. Hij heeft wel toegang nodig tot de "
     "zekeringkast in de kelder. Wie kan hem die dag binnenlaten, en om hoe laat zijn jullie thuis?\n\nLies",
     "work yes", {}, "kitchen"),
    (2, 8, "Tom Claes <tom@brightloop.invalid>", "Review of the billing PR before Thursday?",
     "Hi Sam,\n\nI opened the pull request for the new billing export. It touches the invoice rounding, so I'd like a second "
     "pair of eyes before we merge. Could you review it before Thursday's release?\n\nThanks,\nTom",
     "work yes", {}, None),
    (1, 7, "This Week in Rust <newsletter@twir.invalid>", "This Week in Rust 612",
     "Hello Rustaceans! This week: a new async closure RFC, faster incremental builds, and 14 crates of the week. "
     "Read the full issue on our site.\n\nYou receive this because you subscribed.",
     "newsletter no", {"List-Id": "<twir.newsletter.invalid>", "List-Unsubscribe": "<https://twir.invalid/unsubscribe>"}, None),
    (1, 3, "CI <noreply@ci.brightloop.invalid>", "[brightloop/api] Build failed on main (#4821)",
     "Build #4821 failed on main.\n\nFailed step: integration tests (billing_export_rounding)\nCommit: 9f3c2e1 by Tom Claes\n\n"
     "View the build log for details.",
     "notification no", {}, None),
    (3, 14, "Boekhouding Smet <els@boekhouding-smet.invalid>", "BTW-aangifte Q4: facturen nodig",
     "Dag Sam,\n\nVoor de btw-aangifte van het vierde kwartaal heb ik nog de aankoopfacturen van oktober nodig. "
     "Kan je die tegen 10 december doorsturen? Dan dien ik de aangifte op tijd in.\n\nVriendelijke groeten,\nEls",
     "finance yes", {}, None),
    (4, 19, "Jonas De Wit <jonas.dewit@mail.invalid>", "BBQ zaterdag?",
     "Hey Sam!\n\nWe doen zaterdag een BBQ in de tuin, vanaf 17u. Kom je ook? Laat je even weten of je iemand meebrengt, "
     "dan reken ik op genoeg worsten.\n\nJonas",
     "personal yes", {}, None),
    (2, 6, "Bank Noord <no-reply@banknoord.invalid>", "Uw rekeninguittreksel van oktober staat klaar",
     "Beste klant,\n\nUw rekeninguittreksel van oktober staat klaar in de app van Bank Noord. "
     "U hoeft niets te doen.\n\nMet vriendelijke groeten,\nBank Noord",
     "finance no", {}, None),
    (5, 11, "Camille Laurent <camille@atelier-laurent.invalid>", "Demande de devis pour le site",
     "Bonjour Sam,\n\nNous aimerions refaire le site de notre atelier avant le printemps. Pourriez-vous nous envoyer un devis "
     "pour un site vitrine en français et en néerlandais, avec une page de réservation ?\n\nBien à vous,\nCamille Laurent",
     "work yes", {}, None),
    (3, 9, "App Store <no_reply@apps.invalid>", "Your receipt from the App Store",
     "Receipt\n\nOrder ID: MX9K2LQ\nDate: 30 Oct 2026\nNotes Pro (yearly) €19.99\n\nTotal: €19.99",
     "notification no", {}, None),
    (6, 15, "Nora Jacobs <nora@kleurrijk.invalid>", "Logo files for the new brochure",
     "Hi Sam,\n\nCould you send me the logo in vector format for the brochure?\n\nNora",
     "work yes", {}, "logo"),
    (6, 17, ME, "Re: Logo files for the new brochure",
     "Hi Nora,\n\nHere are the SVG and PDF versions. Let me know if you need anything else.\n\nSam",
     None, {}, "logo"),
    (6, 18, "Nora Jacobs <nora@kleurrijk.invalid>", "Re: Logo files for the new brochure",
     "Perfect, thanks! That's all I needed.\n\nNora",
     "work no", {}, "logo"),
    (1, 12, "Spring Sale <deals@outdoorshop.invalid>", "-40% on all hiking boots this weekend only",
     "Spring is coming! Get 40% off all hiking boots until Sunday. Free delivery from €50.\n\nShop now.",
     "newsletter no", {"List-Unsubscribe": "<mailto:leave@outdoorshop.invalid>"}, None),
    (0, 9, "Pieter Wouters <pieter@brightloop.invalid>", "Standup moved to 10:30 tomorrow",
     "Hi all,\n\nFYI: tomorrow's standup moves to 10:30 because of the client demo. Same room.\n\nPieter",
     "work no", {}, None),
    (0, 7, "Mama <an.peeters@mail.invalid>", "Zondag eten?",
     "Dag schat,\n\nKomen jullie zondag eten? Papa maakt stoofvlees. Laat je weten of jullie om 12u kunnen?\n\nDikke kus,\nMama",
     "personal yes", {}, None),
    (4, 8, "Energie Plus <factuur@energieplus.invalid>", "Uw factuur van november",
     "Beste Sam Peeters,\n\nUw factuur van november bedraagt € 142,30 en wordt op 20 november van uw rekening gedomicilieerd.\n\n"
     "Energie Plus",
     "finance no", {}, None),
    (0, 4, "Hugo Martin <hugo@transports-martin.invalid>", "Livraison du 12 novembre",
     "Bonjour,\n\nLa livraison prévue le 12 novembre est confirmée entre 8h et 12h. Aucune action n'est requise de votre part.\n\n"
     "Cordialement,\nHugo Martin",
     "notification no", {}, None),
]


def main(out):
    os.makedirs(out, exist_ok=True)
    threads = {}
    for i, (days, hour, sender, subject, body, expect, extra, key) in enumerate(MAIL, 1):
        when = (NOW - datetime.timedelta(days=days)).replace(hour=hour, minute=(i * 7) % 60, second=0, microsecond=0)
        m = EmailMessage()
        m["From"] = sender
        m["To"] = "Lies Vermeulen <lies@vermeulen-bouw.invalid>" if sender == ME and key == "kitchen" else (
            "Nora Jacobs <nora@kleurrijk.invalid>" if sender == ME else ME)
        m["Subject"] = subject
        m["Date"] = email.utils.format_datetime(when)
        mid = f"<jolta-ai-fixture-{i}@jolta.invalid>"
        m["Message-ID"] = mid
        if key:
            refs = threads.setdefault(key, [])
            if refs:
                m["In-Reply-To"] = refs[-1]
                m["References"] = " ".join(refs)
            refs.append(mid)
        for name, value in extra.items():
            m[name] = value
        if expect:
            m["X-Jolta-Expect"] = expect
        m.set_content(body)
        with open(os.path.join(out, f"{i:02d}.eml"), "wb") as f:
            f.write(bytes(m))
    print(f"{len(MAIL)} messages in {out}")


main(sys.argv[1])
