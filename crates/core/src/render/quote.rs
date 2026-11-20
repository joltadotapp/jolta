#[cfg(test)]
mod tests {
    use crate::render::{MessageView, render_thread};
    use crate::smime::Protection;

    const ANN: &str = "<div>Hi Bob,</div><div><br></div><div>Can you send me the Q3 numbers before Thursday? The board meets on Friday morning.</div>\
        <div><br></div><div>Thanks,</div><div>Ann</div>";

    const ANN_TEXT: &str = "Hi Bob,\n\nCan you send me the Q3 numbers before Thursday? The board meets on Friday\nmorning.\n\nThanks,\nAnn";

    fn html(body: &str) -> Vec<u8> {
        format!("From: a@example.com\r\nSubject: Q3 numbers\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{body}\r\n").into_bytes()
    }

    fn text(body: &str) -> Vec<u8> {
        format!("From: a@example.com\r\nSubject: Q3 numbers\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{}\r\n", body.replace('\n', "\r\n")).into_bytes()
    }

    /// Each message's body as rendered: what shows, and what a quote toggle hides.
    fn rendered(raws: &[Vec<u8>]) -> Vec<(String, String)> {
        let views: Vec<MessageView> = raws
            .iter()
            .enumerate()
            .map(|(i, raw)| MessageView {
                id: i as i64 + 1,
                from: "a",
                to: "b",
                date: "d",
                snippet: "s",
                raw,
                open: true,
                vip: false,
                protection: &Protection::None,
                cards: &[],
            })
            .collect();
        let html = render_thread(&views, "").html;
        (1..=raws.len())
            .map(|id| {
                let start = html.find(&format!("id=\"m{id}\"")).unwrap();
                let end = html[start..].find("<details class=\"jolta-message\"").map_or(html.len(), |i| start + i);
                let body = &html[start..end];
                match body.find("<details class=\"jolta-quoted\">") {
                    Some(fold) => {
                        let close = fold + body[fold..].find("</details>").unwrap() + "</details>".len();
                        (format!("{}{}", &body[..fold], &body[close..]), body[fold..close].to_string())
                    }
                    None => (body.to_string(), String::new()),
                }
            })
            .collect()
    }

    fn assert_folds(raws: &[Vec<u8>], shown: &[&str], hidden: &[&str]) {
        let last = rendered(raws).pop().unwrap();
        assert!(!last.1.is_empty(), "the quote folds:\n{}", last.0);
        for s in shown {
            assert!(last.0.contains(s), "{s:?} stays shown:\n{}\n---\n{}", last.0, last.1);
        }
        for h in hidden {
            assert!(last.1.contains(h) && !last.0.contains(h), "{h:?} folds away:\n{}\n---\n{}", last.0, last.1);
        }
        assert!(
            last.1.starts_with("<details class=\"jolta-quoted\"><summary aria-label=\"Show quoted text\">"),
            "the toggle has a label: {}",
            last.1
        );
    }

    fn assert_shown(raws: &[Vec<u8>]) {
        for (i, (_, hidden)) in rendered(raws).iter().enumerate() {
            assert!(hidden.is_empty(), "message {} folds nothing: {hidden}", i + 1);
        }
    }

    #[test]
    fn a_gmail_quote_of_an_earlier_message_folds_with_its_attribution() {
        let reply = html(&format!(
            "<div dir=\"ltr\">Here they are, see the second tab.<div><br></div><div>Bob</div></div><br>\
             <div class=\"gmail_quote gmail_quote_container\"><div dir=\"ltr\" class=\"gmail_attr\">On Mon, 3 Nov 2026 at 09:12, Ann Example \
             &lt;<a href=\"mailto:ann@example.com\">ann@example.com</a>&gt; wrote:<br></div>\
             <blockquote class=\"gmail_quote\" style=\"margin:0px 0px 0px 0.8ex;border-left:1px solid rgb(204,204,204);padding-left:1ex\">{ANN}</blockquote></div>"
        ));
        assert_folds(&[html(ANN), reply], &["Here they are", "Bob"], &["Ann Example", "board meets"]);
    }

    #[test]
    fn an_older_dutch_gmail_quote_folds() {
        let reply = html(&format!(
            "<div>Hier zijn ze.</div><div>Bob</div><br><div class=\"gmail_quote\">Op 3 november 2026 09:12 schreef Ann Example \
             <span dir=\"ltr\">&lt;<a href=\"mailto:ann@example.com\">ann@example.com</a>&gt;</span> het volgende:<br>\
             <blockquote class=\"gmail_quote\" style=\"margin:0 0 0 .8ex;border-left:1px #ccc solid;padding-left:1ex;\">{ANN}</blockquote></div>"
        ));
        assert_folds(&[html(ANN), reply], &["Hier zijn ze"], &["schreef Ann Example", "board meets"]);
    }

    #[test]
    fn an_apple_mail_quote_folds_with_its_attribution() {
        let mac = html(&format!(
            "<div>Here they are, see the second tab.</div><div>Bob</div><div><br><div><div>On 3 Nov 2026, at 09:12, Ann Example \
             &lt;ann@example.com&gt; wrote:</div><br class=\"Apple-interchange-newline\"><blockquote type=\"cite\">{ANN}</blockquote></div></div>"
        ));
        assert_folds(&[html(ANN), mac], &["Here they are"], &["Ann Example", "board meets"]);

        let phone = html(&format!(
            "<div dir=\"ltr\">Here they are, see the second tab.</div><div dir=\"ltr\">Bob</div><div dir=\"ltr\"><br>\
             <blockquote type=\"cite\">On 3 Nov 2026, at 09:12, Ann Example &lt;ann@example.com&gt; wrote:<br><br></blockquote></div>\
             <blockquote type=\"cite\"><div dir=\"ltr\">{ANN}</div></blockquote>"
        ));
        assert_folds(&[html(ANN), phone], &["Here they are"], &["Ann Example", "board meets"]);
    }

    #[test]
    fn a_thunderbird_quote_folds_with_its_cite_prefix() {
        let reply = html(&format!(
            "<p>Here they are, see the second tab.</p><p>Bob</p><div class=\"moz-cite-prefix\">On 03/11/2026 09:12, Ann Example wrote:<br></div>\
             <blockquote type=\"cite\" cite=\"mid:q3@example.com\">{ANN}</blockquote>"
        ));
        assert_folds(&[html(ANN), reply], &["Here they are"], &["Ann Example", "board meets"]);
    }

    #[test]
    fn an_outlook_web_quote_folds_with_its_header() {
        let reply = html(&format!(
            "<div style=\"font-family:Aptos\">Here they are, see the second tab.</div><div>Bob</div><div id=\"appendonsend\"></div>\
             <hr style=\"display:inline-block;width:98%\" tabindex=\"-1\"><div id=\"divRplyFwdMsg\" dir=\"ltr\">\
             <font face=\"Calibri, sans-serif\" style=\"font-size:11pt\" color=\"#000000\"><b>From:</b> Ann Example &lt;ann@example.com&gt;<br>\
             <b>Sent:</b> Monday, November 3, 2026 9:12 AM<br><b>To:</b> Bob Example &lt;bob@example.com&gt;<br><b>Subject:</b> Q3 numbers</font>\
             <div>&nbsp;</div></div><div>{ANN}</div>"
        ));
        assert_folds(&[html(ANN), reply], &["Here they are", "Bob"], &["Ann Example", "Q3 numbers</font>", "board meets", "<hr"]);
    }

    fn outlook_desktop(labels: [&str; 4]) -> Vec<u8> {
        let [from, sent, to, subject] = labels;
        let quoted: String = ANN
            .split("</div>")
            .filter(|l| !l.is_empty())
            .map(|l| format!("<p class=\"MsoNormal\">{}</p>", l.trim_start_matches("<div>").replace("<br>", "&nbsp;")))
            .collect();
        html(&format!(
            "<div class=\"WordSection1\"><p class=\"MsoNormal\">Here they are, see the second tab.</p><p class=\"MsoNormal\">Bob</p>\
             <p class=\"MsoNormal\">&nbsp;</p><div><div style=\"border:none;border-top:solid #E1E1E1 1.0pt;padding:3.0pt 0cm 0cm 0cm\">\
             <p class=\"MsoNormal\"><b>{from}</b> Ann Example &lt;ann@example.com&gt; <br><b>{sent}</b> 3 November 2026 09:12<br>\
             <b>{to}</b> Bob Example &lt;bob@example.com&gt;<br><b>{subject}</b> Q3 numbers</p></div></div><p class=\"MsoNormal\">&nbsp;</p>\
             {quoted}</div>"
        ))
    }

    #[test]
    fn an_outlook_quote_folds_with_its_dutch_header() {
        let reply = outlook_desktop(["Van:", "Verzonden:", "Aan:", "Onderwerp:"]);
        assert_folds(&[html(ANN), reply], &["Here they are", "Bob"], &["Verzonden:", "Ann Example", "board meets"]);
    }

    #[test]
    fn an_outlook_quote_folds_with_its_header_in_english_french_or_german() {
        for labels in [["From:", "Sent:", "To:", "Subject:"], ["De&nbsp;:", "Envoyé&nbsp;:", "À&nbsp;:", "Objet&nbsp;:"], ["Von:", "Gesendet:", "An:", "Betreff:"]] {
            assert_folds(&[html(ANN), outlook_desktop(labels)], &["Here they are"], &[labels[1], "Ann Example", "board meets"]);
        }
    }

    #[test]
    fn an_original_message_line_folds_with_what_follows() {
        let reply = text(&format!(
            "Here they are, see the second tab.\n\nBob\n\n-----Original Message-----\nFrom: Ann Example <ann@example.com>\n\
             Sent: Monday, November 3, 2026 9:12 AM\nTo: Bob Example <bob@example.com>\nSubject: Q3 numbers\n\n{ANN_TEXT}"
        ));
        assert_folds(&[html(ANN), reply], &["Here&#32;they&#32;are", "Bob"], &["Original&#32;Message", "board&#32;meets"]);

        let dutch = text(&format!(
            "Hier zijn ze.\n\n-----Oorspronkelijk bericht-----\nVan: Ann Example <ann@example.com>\nVerzonden: maandag 3 november 2026 09:12\n\
             Aan: Bob Example <bob@example.com>\nOnderwerp: Q3 numbers\n\n{ANN_TEXT}"
        ));
        assert_folds(&[html(ANN), dutch], &["Hier&#32;zijn&#32;ze"], &["Oorspronkelijk&#32;bericht", "board&#32;meets"]);
    }

    #[test]
    fn a_yahoo_quote_folds() {
        let reply = html(&format!(
            "<div class=\"ydp1\"><div>Here they are, see the second tab.</div><div>Bob</div></div>\
             <div id=\"yahoo_quoted_4242\" class=\"yahoo_quoted\"><div style=\"font-family:Helvetica\">\
             <div>On Monday, November 3, 2026 at 09:12:00 CET, Ann Example &lt;ann@example.com&gt; wrote:</div><div><br></div><div><br></div>\
             <div><div dir=\"ltr\">{ANN}</div></div></div></div>"
        ));
        assert_folds(&[html(ANN), reply], &["Here they are"], &["Ann Example", "board meets"]);
    }

    #[test]
    fn a_plain_text_quote_folds_with_its_attribution() {
        let quoted: String = ANN_TEXT.lines().map(|l| if l.is_empty() { ">\n".to_string() } else { format!("> {l}\n") }).collect();
        let reply = text(&format!("Here they are, see the second tab.\n\nBob\n\nOn Mon, 3 Nov 2026 at 09:12, Ann Example <ann@example.com> wrote:\n\n{quoted}"));
        assert_folds(&[html(ANN), reply], &["Here&#32;they&#32;are", "Bob"], &["Ann&#32;Example", "board&#32;meets"]);
    }

    #[test]
    fn a_quote_of_a_quote_folds_when_both_were_shown_before() {
        let bob = format!(
            "<div dir=\"ltr\">Here they are, see the second tab.<div>Bob</div></div><br><div class=\"gmail_quote\">\
             <div class=\"gmail_attr\">On Mon, 3 Nov 2026 at 09:12, Ann Example wrote:<br></div><blockquote class=\"gmail_quote\">{ANN}</blockquote></div>"
        );
        let ann = format!(
            "<div dir=\"ltr\">Got them, thank you.</div><br><div class=\"gmail_quote\">\
             <div class=\"gmail_attr\">On Mon, 3 Nov 2026 at 10:30, Bob Example wrote:<br></div><blockquote class=\"gmail_quote\">{bob}</blockquote></div>"
        );
        let raws = [html(ANN), html(&bob), html(&ann)];
        assert_folds(&raws, &["Got them"], &["Here they are", "board meets"]);
        assert!(!rendered(&raws)[0].0.contains("jolta-quoted"));
    }

    #[test]
    fn answers_typed_into_the_quote_stay_shown() {
        let between = html(
            "<div>Answers below.</div><div><br><blockquote type=\"cite\"><div>On 3 Nov 2026, at 09:12, Ann Example wrote:</div>\
             <br class=\"Apple-interchange-newline\"><div>Hi Bob,</div><div><br></div>\
             <div>Can you send me the Q3 numbers before Thursday? The board meets on Friday morning.</div></blockquote></div>\
             <div>Yes, attached. Friday works.</div><blockquote type=\"cite\"><div>Thanks,</div><div>Ann</div></blockquote>",
        );
        assert_shown(&[html(ANN), between]);

        let inside = html(
            "<div>See my answer in red.</div><div class=\"gmail_quote\"><div class=\"gmail_attr\">On Mon, 3 Nov 2026, Ann Example wrote:</div>\
             <blockquote class=\"gmail_quote\"><div>Hi Bob,</div><div><br></div>\
             <div>Can you send me the Q3 numbers before Thursday? The board meets on Friday morning.</div>\
             <div style=\"color:red\">Yes, they are attached.</div><div><br></div><div>Thanks,</div><div>Ann</div></blockquote></div>",
        );
        assert_shown(&[html(ANN), inside]);

        let plain = text(
            "On Mon, 3 Nov 2026 at 09:12, Ann Example <ann@example.com> wrote:\n> Hi Bob,\n>\n\
             > Can you send me the Q3 numbers before Thursday? The board meets on Friday\n> morning.\n\nYes, attached.\n\n> Thanks,\n> Ann\n",
        );
        assert_shown(&[html(ANN), plain]);
    }

    #[test]
    fn a_forward_of_mail_outside_the_conversation_stays_shown() {
        let forward = html(
            "<div>Bob, see what Carol wrote about the numbers.</div><div id=\"appendonsend\"></div><hr style=\"display:inline-block;width:98%\">\
             <div id=\"divRplyFwdMsg\" dir=\"ltr\"><font face=\"Calibri, sans-serif\"><b>From:</b> Carol Example &lt;carol@example.org&gt;<br>\
             <b>Sent:</b> Friday, October 31, 2026 4:02 PM<br><b>To:</b> Ann Example &lt;ann@example.com&gt;<br><b>Subject:</b> Audit</font></div>\
             <div>Ann, the auditors need the Q3 numbers by Thursday as well. Please send them both.</div>",
        );
        assert_shown(&[html(ANN), forward]);
    }

    #[test]
    fn the_first_message_never_folds() {
        let reply = html(&format!(
            "<div>Here they are.</div><div class=\"gmail_quote\"><div class=\"gmail_attr\">On Mon, 3 Nov 2026, Ann Example wrote:</div>\
             <blockquote class=\"gmail_quote\">{ANN}</blockquote></div>"
        ));
        assert_shown(std::slice::from_ref(&reply));
        assert_shown(&[reply, html("<div>Thanks!</div>")]);
    }
}
