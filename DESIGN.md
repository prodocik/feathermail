---
name: Feather Mail
description: Cool-paper three-pane inbox — one blue voice, tonal selection, hairline structure.
colors:
  paper-sidebar: "#FAFAFC"
  paper-pane: "#FFFFFF"
  paper-recess: "#F5F6F8"
  paper-wash: "#F1F5FD"
  paper-selected: "#E5ECFD"
  ink: "#0B0C0E"
  ink-secondary: "#5B6270"
  ink-tertiary: "#697080"
  hairline: "#EEF0F3"
  stroke-soft: "#E6E8EE"
  accent: "#1A64FC"
  accent-deep: "#1557E0"
  accent-text: "#FFFFFF"
  link: "#1A58F4"
  avatar-blue: "#3B82F6"
  studio: "#C5CAD2"
  folder-work: "#47CC50"
  folder-personal: "#9451F4"
  folder-projects: "#FB954A"
  folder-receipts: "#4181F3"
  folder-travel: "#2DD2E0"
  provider-yandex: "#FC3F1D"
  dark-bg: "#111315"
  dark-panel: "#181B1F"
  dark-text: "#F3F4F6"
  dark-secondary: "#9CA3AF"
  dark-accent: "#60A5FA"
typography:
  display:
    fontFamily: "Inter, Adwaita Sans, system-ui, sans-serif"
    fontSize: "26px"
    fontWeight: 650
    lineHeight: 1.28
    letterSpacing: "-0.02em"
  headline:
    fontFamily: "Inter, Adwaita Sans, system-ui, sans-serif"
    fontSize: "22px"
    fontWeight: 650
    lineHeight: 1.3
    letterSpacing: "-0.015em"
  title:
    fontFamily: "Inter, Adwaita Sans, system-ui, sans-serif"
    fontSize: "16px"
    fontWeight: 650
    lineHeight: 1.35
    letterSpacing: "-0.01em"
  body:
    fontFamily: "Inter, Adwaita Sans, system-ui, sans-serif"
    fontSize: "16px"
    fontWeight: 400
    lineHeight: 1.7
    letterSpacing: "normal"
  body-sm:
    fontFamily: "Inter, Adwaita Sans, system-ui, sans-serif"
    fontSize: "15px"
    fontWeight: 400
    lineHeight: 1.45
    letterSpacing: "normal"
  label:
    fontFamily: "Inter, Adwaita Sans, system-ui, sans-serif"
    fontSize: "13px"
    fontWeight: 500
    lineHeight: 1.25
    letterSpacing: "0.01em"
  marketing-display:
    fontFamily: "Inter, Adwaita Sans, system-ui, sans-serif"
    fontSize: "clamp(52px, 7vw, 92px)"
    fontWeight: 650
    lineHeight: 0.98
    letterSpacing: "-0.065em"
  marketing-heading:
    fontFamily: "Inter, Adwaita Sans, system-ui, sans-serif"
    fontSize: "clamp(38px, 5vw, 66px)"
    fontWeight: 650
    lineHeight: 1.04
    letterSpacing: "-0.05em"
rounded:
  sm: "6px"
  md: "12px"
  lg: "16px"
  full: "999px"
spacing:
  2xs: "4px"
  xs: "8px"
  sm: "12px"
  md: "16px"
  lg: "20px"
  xl: "24px"
  2xl: "32px"
  3xl: "40px"
  4xl: "48px"
  5xl: "64px"
components:
  button-primary:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.accent-text}"
    typography: "{typography.title}"
    rounded: "{rounded.full}"
    padding: "14px 22px"
    height: "48px"
  button-primary-hover:
    backgroundColor: "{colors.accent-deep}"
    textColor: "{colors.accent-text}"
    rounded: "{rounded.full}"
  button-compose:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.accent-text}"
    typography: "{typography.title}"
    rounded: "{rounded.full}"
    padding: "0 14px"
    height: "44px"
  button-icon:
    backgroundColor: "transparent"
    textColor: "{colors.ink-secondary}"
    rounded: "{rounded.md}"
    padding: "10px"
    size: "40px"
  input-search:
    backgroundColor: "{colors.paper-recess}"
    textColor: "{colors.ink}"
    typography: "{typography.body-sm}"
    rounded: "{rounded.full}"
    padding: "0 16px 0 44px"
    height: "48px"
  nav-item:
    backgroundColor: "transparent"
    textColor: "{colors.ink}"
    typography: "{typography.body-sm}"
    rounded: "{rounded.md}"
    padding: "8px 14px"
    height: "40px"
  nav-item-active:
    backgroundColor: "{colors.paper-selected}"
    textColor: "{colors.accent}"
    typography: "{typography.title}"
    rounded: "{rounded.md}"
    padding: "8px 14px"
    height: "40px"
  message-row:
    backgroundColor: "{colors.paper-pane}"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
    padding: "10px 10px 10px 16px"
    height: "96px"
  message-row-selected:
    backgroundColor: "{colors.paper-wash}"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
    padding: "10px 10px 10px 16px"
    height: "96px"
  chip-folder:
    backgroundColor: "{colors.paper-recess}"
    textColor: "{colors.ink-secondary}"
    typography: "{typography.label}"
    rounded: "{rounded.full}"
    padding: "2px 8px"
  reply-bar:
    backgroundColor: "{colors.paper-pane}"
    textColor: "{colors.ink-secondary}"
    typography: "{typography.body-sm}"
    rounded: "{rounded.md}"
    padding: "0 14px"
    height: "40px"
---

# Design System: Feather Mail

Этот файл — публичный визуальный контракт приложения: роли, токены и правила компонентов.

Источник характера — прохладная бумага, спокойная трёхпанельная структура и один электрически-синий акцент. **Плотность и кегль** комфортные, с курсом на воздух [Kanmail](https://kanmail.io/) — не на его канбан-колонки. Тёмная тема использует те же семантические роли.

Оконный хром на презентационных скриншотах — это **презентация кадра**, не Linux-хром. GTK рисует свои декорации. Контракт ниже описывает **внутренности** трёх панелей.

**Знак приложения:** [`icon.png`](icon.png) — белое перо поверх конверта на синем squircle. Канонический файл, не генерировать замену. В шелле Inbox не дублировать (мокап без логотипа в хроме). Welcome / About / `.desktop` / README — да.

## Overview

**Creative North Star: "The Cool Paper Inbox"**

Почта как стопка прохладной бумаги на столе, а не как дашборд и не как Gmail. Фон — почти белый с едва голубым сдвигом. Единственный цветной голос — электрический синий акцента: Compose, непрочитанное, активный Inbox, ссылки. Всё остальное — чернила и тишина.

Плотность асимметрична: сайдбар — индекс; список Comfortable — бумажные карточки с зазором на recess (курс на воздух Kanmail, не канбан); правая — читальный зал с широкими полями и межстрочным 1.7. Состояния (выбранная строка, выбранная папка) — это **размытая заливка акцента**, не обводка и не тень. Иконки — тонкий stroke 1.5px, серо-синие, никогда не заливные, кроме статусных точек.

**Key Characteristics:**

- Холодная бумага, не тёплый cream и не нейтральный Material gray.
- Один акцент на ≤10% экрана.
- Выбор = wash, не ring.
- Структура = hairline 1px, внутри приложения теней нет.
- Полные пилюли только у Compose и Search; строки и панели — 12px.
- Список держит sender, subject и две строки preview, не однострочный Gmail-scan.
- Comfortable — воздух между строками, как у карточек Kanmail; не канбан и не packed Gmail.

## Colors

Палитра **Restrained**: поле нейтралей + один насыщенный синий. Цветные точки папок — локальные маркеры, не вторая палитра продукта.

### Primary

- **Signal Blue** (`accent`, `#1A64FC`): Compose, unread-точка, счётчик Inbox, активный пункт сайдбара, фокус. На мокапе заливка кнопки читается как `(26, 100, 252)` — чуть электричнее, чем `#2563EB` из ТЗ. Использовать снятое.
- **Signal Blue Deep** (`accent-deep`, `#1557E0`): hover/pressed Compose.
- **Link Blue** (`link`, `#1A58F4`): подчёркнутые ссылки в теле письма. Не красить ими хром.

### Provider marks (вне хрома)

- **Yandex mark** (`provider-yandex`, `#FC3F1D`): официальный знак только на preset-кнопке Yandex. Это узнаваемый знак провайдера, не второй акцент интерфейса.

### Neutral

- **Sidebar Paper** (`paper-sidebar`, `#FAFAFC`): левая колонка. Чуть холоднее и темнее белых панелей — так читается глубина без тени.
- **Pane Paper** (`paper-pane`, `#FFFFFF`): строки списка и превью.
- **Recess** (`paper-recess`, `#F5F6F8`): колонка списка (стол под карточками), Search field, chip «Inbox», иконка `/`.
- **Selection Wash** (`paper-wash`, `#F1F5FD`): выбранная строка письма (~8% акцента).
- **Nav Selected** (`paper-selected`, `#E5ECFD`): активный Inbox в сайдбаре (плотнее wash).
- **Ink** (`ink`, `#0B0C0E`): заголовки и тело. На мокапе глифы письма почти `#000`; для GTK/a11y держим чуть мягче, не `#15171A` из ТЗ — тот серее, чем кадр.
- **Ink Secondary** (`ink-secondary`, `#5B6270`): подписи, время, иконки тулбара, email в сайдбаре (~`#747677` на кадре; канон ТЗ совпал достаточно). T-160: затемнён с `#6B7280` до WCAG AA 4.5:1 на всех фонах бумаги (paper-pane/sidebar/recess/wash/selected).
- **Ink Tertiary** (`ink-tertiary`, `#697080`): preview-строка, плейсхолдер Reply, секция «Folders». T-160: затемнён с `#9AA0AB` до WCAG AA 4.5:1 на бумаге; единственное исключение — на `paper-selected` контраст 4.20:1, выше порога 3:1 для крупного текста/UI.
- **Hairline** (`hairline`, `#EEF0F3`): вертикальные сплиттеры сайдбар/список/превью.
- **Stroke Soft** (`stroke-soft`, `#E6E8EE`): обводка Reply-бара.

### Tertiary (только маркеры папок)

Точки 8px в секции Folders, как на кадре. Не использовать эти цвета в хроме.

- Work `#47CC50` · Personal `#9451F4` · Projects `#FB954A` · Receipts `#4181F3` · Travel `#2DD2E0`

### Dark (не с мокапа — ТЗ §45)

- Фон `#111315`, панели `#181B1F`, текст `#F3F4F6`, secondary `#9CA3AF`, акцент `#60A5FA`.
- Те же роли: сайдбар чуть светлее фона, выбор = акцент ~12% opacity, hairline = белая 8% opacity. Не инвертировать пилюли в outline-кнопки.

**The One Voice Rule.** На любом экране насыщенный синий занимает ≤10% площади. Если хочется «оживить» панель — ослабить, не добавить второй акцент.

**The Wash-Not-Outline Rule.** Выбранность — заливка `paper-wash` / `paper-selected`. Никаких 2px blue rings вокруг строк.

## Typography

**Display Font:** Inter (fallback: Adwaita Sans, system-ui)
**Body Font:** тот же Inter
**Label/Mono Font:** Inter; моно только в Diagnostics, не в Inbox

**Character:** геометрический гротеск с высокой x-height. На кадре это Inter/SF-подобный UI-sans: плотный, не editorial serif, не rounded «friendly» лицо. Один файл на все роли; иерархия — вес и кегль, не смена семьи.

### Hierarchy

- **Display** (650, 26px, 1.28, −0.02em): subject открытого письма.
- **Headline** (650, 22px, 1.3): «Inbox» над списком.
- **Title** (650, 16px, 1.35): имя аккаунта, sender непрочитанного, подпись Compose.
- **Body** (400, 16px, 1.7): тело письма. Межстрочный воздух — часть читального зала.
- **Body-sm** (400, 15px, 1.45): subject и preview в списке; пункты сайдбара.
- **Label** (500, 13px, 1.25): время, «Folders», «Today/Yesterday», счётчики.
- **Marketing display** (650, 52–92px fluid, 0.98): только hero публичной страницы; в приложении не используется.
- **Marketing heading** (650, 38–66px fluid, 1.04): только секционные заголовки публичной страницы.

Непрочитанное в списке: sender и subject тем же кеглем, но **650**. Прочитанное: 400 / secondary на preview.

Строка списка идёт на ступень ниже этой шкалы (решение T-097(1)): `.msg-sender`/`.msg-subject` — 14px (не Title 16px и не Body-sm 15px), `.msg-preview` — 13px (не Body-sm 15px), `.msg-time` — 12px (не Label 13px). Шкала Hierarchy описывает приложение в целом, список — сознательное исключение из неё, а не расхождение с ней.

**The One Face Rule.** Никакого второго шрифта в оболочке. HTML-письмо внутри WebKit может нести свои лица — это контент, не хром.

## Layout

Три колонки в одном окне, верхняя полоса действий общая.

```text
| sidebar 264–288 | list 400–460 | preview 1fr |
|                 |              |             |
| 80px top bar spanning list+preview: Compose · Search · icon cluster
```

- Сайдбар не скроллит вместе с письмами. Account block сверху, секция **All Inbox** (системные ящики Inbox…Trash), затем **Folders** + `+`, settings/theme прибиты к низу. All Inbox — заголовок раздела текущего аккаунта, не сводный ящик всех аккаунтов.
- Вертикальные сплиттеры — hairline, без drag-handle визуала (resize можно, ручку не рисовать).
- Ритм **8px**. Внутренние поля колонок 20–32px. Nav item 40px, gap 4px. Колонка списка — `paper-recess` (в dark — чуть темнее панелей); строки — `paper-pane` 96px (76px контента: sender 19 + subject 19 + preview 34 + два зазора 2px, плюс 10px vertical padding) + **8px gap** (T-097(1,2)). Внутренние поля строки — 10px сверху/снизу/справа и 16px слева (T-099: звезда и hover-плашка отступают от угла карточки на 10px, текст слева не прижат к краю). Выбранная строка — 9% акцента на pane, чтобы карточка не слипалась со столом. Зазор читается как бумага стола, не как тень карточки.
- Осознанные исключения из ритма 8px (T-097(4)/T-099): `.msg-row` padding `10px 10px 10px 16px`, `.quick-actions` `margin-top: 10px; margin-right: 22px` (12+10), `.pull-refresh-bubble` padding `7px`. Не «выравнивать» их обратно на сетку без отдельной задачи.
- Группы дат («Today») — 13px label, воздух сверху от предыдущего блока, 8px снизу до первой строки. Не карточки-секции.
- Превью: subject и мета в ~32px полях; тело 16px с шириной меры ~60–70ch, не на всю колонку в 4K.
- Compact density (настройка): row 72px, nav 36px, gap 4px, те же радиусы. **Comfortable = дефолт, воздух как у Kanmail.** Compact ≈ прежний packed-мокап.
- Масштаб 100–200% через Settings → Interface scale (`--fm-scale` на типе); сетка остаётся 8px логических (padding / min-height в px).

**The Index/Reading Split.** Список — каталог с зазором между строками (карточки по воздуху, не канбан-доска). Превью — набор с ещё более широкими полями. Не уплотнять превью до плотности списка.

## Elevation & Depth

Система **тональная, не теневая**. Внутри окна теней нет. Глубина = сайдбар `#FAFAFC` против панелей `#FFFFFF` + wash выбора + hairline.

Внешняя тень окна на `#C5CAD2` в мокапе — артефакт презентации. В GTK её нет.

**The No Inner Shadow Rule.** Ни `box-shadow` на строках, ни card elevation, ни blur на панелях. Hover строки — тот же wash на 50% opacity или слабее, не подъём.

## Shapes

Мягкие, но не игрушечные.

- **Пилюля (`full`)**: Compose, Search, `/` hint, avatar, unread-dot (8px), folder-dot (8px), star hit-target.
- **12px (`md`)**: выбранная строка, nav item, Reply bar, context menu, toast.
- **6px (`sm`)**: мелкие chip (метка «Inbox» у subject).
- Сплиттеры прямые, без скругления панелей относительно друг друга — колонки впритык, скругляется только интерактивный объект внутри.

**The Two Radii Rule.** Либо полностью круглое (действие/поиск/статус), либо 12px (контентные блоки). Промежуточные 4px/20px не вводить.

## Components

### Buttons

Характер: одна громкая пилюля, остальные — тихие иконки.

- **Primary (Compose):** компактная пилюля 44px высотой, `#1A64FC`, белый текст 16/650, иконка карандаша 16px слева, gap 8px. Отдельна от широких primary-кнопок диалогов, чтобы не теснить Search и toolbar. Hover `#1557E0`. Нет тени, нет градиента.
- **Icon buttons** (архив / unread / snooze / read / more): 40px hit, stroke-иконка 20px цвета `ink-secondary`. Hover — `paper-recess` круг/скругление 12px, не заливка акцентом.
- **Ghost text** (Filter): 15px secondary + маленькая иконка, без рамки.
- **Add account presets:** Google, Microsoft и Yandex — узнаваемый знак 20px слева от текста; Other IMAP — нейтральный envelope stroke 20px в `ink-secondary`. Все четыре кнопки открывают одну ручную IMAP/SMTP-форму, только первые три предварительно заполняют серверы, порты и TLS. Все сохраняют одинаковые 48px height и 10px icon/text gap; логотипы не превращаются в новый акцент оболочки.
- **Password help:** компактная круглая кнопка `?` стоит в заголовке поля Password. Она открывает лёгкий popover с коротким объяснением и официальными внешними ссылками, не уводя человека в отдельный экран и не блокируя ручную форму.

### Search

- Высота 48px, пилюля, заливка `paper-recess`, без обводки в покое.
- Лупа 16px secondary слева, плейсхолдер «Search mail» 15px tertiary.
- Справа отдельная **клавишная пилюля** `/` — ещё светлее, ~30×30, 13px label.
- Focus: ринга нет (T-097(5)). Клавиатурная обратная связь — accent-подчёркивание активного поля Compose (`.compose-grid entry.compose-field:focus-within`) и выделение строки списка из selection-модели.

### Sidebar navigation

- Пункт: иконка 18px stroke + label 15px + опциональный счётчик справа, 40px высотой.
- Active: `paper-selected` на всю ширину колонки минус 12px margin, радиус 12px, иконка и текст `accent`, счётчик `accent` 13/650.
- Inactive: ink, счётчик tertiary.
- Секция «All Inbox»: тот же label, без `+`. Под ней системные ящики.
- Секция «Folders»: label 13px tertiary, `+` opposite. Цветная точка 8px, не иконка. В `All accounts` секция скрыта целиком: custom folder принадлежит одному ящику, а в unified-режиме остаются только Inbox, Sent, Starred и Trash.

### Загрузка почты (низ сайдбара)

- Состояние загрузки живёт **внизу левой колонки**, над кнопками
  настроек и темы, и нигде больше: над списком писем ничего про
  «фетчинг» не стоит — там читают почту.
- Строка 12px secondary («Fetching mail… 91%») и под ней трек 3px:
  `paper-recess` жёлоб, `accent` заливка, радиус 2px. Никакой рамки,
  никакой анимации сверх самой заливки.
- Процент показывается только когда он есть — это доля незакрытого
  первичного бэкфилла по всем ящикам сразу. Обычный короткий проход
  говорит «Fetching mail…» и трек не рисует: доли у него нет, а
  пустой бар читается как «застряло».
- Блок исчезает, когда качать нечего. Он про весь процесс, а не про
  папку на экране.

### Account switcher

- Имя 16/650 ink, email 13px secondary под ним, шеврон справа. Без аватара в мокапе — не добавлять.

### Message row

- Слева unread-dot 8px (`accent` непрочитанное / `ink-tertiary` прочитанное).
- Sender 16/650 (unread) или 16/500 (read).
- Время 13px tertiary, верхний правый угол строки.
- Subject 15px, одна строка; пустой subject честно показывает `(No subject)`.
- Preview 15px tertiary, две строки с ellipsis после второй. Это текст письма, а не его разметка: ни тегов, ни адресов картинок, ни рядов `&` из их параметров (T-134). Превью — сохранённая проекция, поэтому исправление парсера тянет за собой пересчёт уже записанных строк, а не только новых.
- Star 16px outline tertiary в правом верхнем углу; заполняется accent при starred.
- Вложение: paperclip 16px tertiary слева от времени, только если есть файл.
- Пользовательский label (не Inbox/Sent/…) — chip на верхней строке, тот же recess pill что у превью.
- High importance — emblem 12px accent под unread-dot. Low/Normal не рисуются.
- Тред из нескольких писем: sender «Name · N» (D22).
- Selected: 9% акцента на `paper-pane` (карточка не слипается с recess-столом), радиус 12px, не на всю колонку в край.
- Hover quick actions появляются справа, не сдвигая текст.

### Message preview

- Subject = Display. Справа chip папки (recess pill) + star + more.
- Строка отправителя: avatar 40px (`avatar-blue` + буква), имя 16/650, `<email>` secondary, «to me» 15px tertiary, время 13px tertiary, reply/more иконки.
- Тело: 16/1.7 ink. Ссылки `link` + underline.
- HTML-контент живёт в изолированном WebKit: хром вокруг него остаётся этим DS, содержимое — нет.
- Письмо показывается **так, как его написал отправитель** (T-141): у `multipart/alternative` берётся санитизированная HTML-половина, а не текстовая. В текстовой картинка превращается в свой `alt` и адрес в скобках, и владелец увидел ровно это: «Логотип Jira [https://…png]» вместо логотипа. «Prefer plain text» в Privacy остаётся — это выбор читателя, а не значение по умолчанию.
- Приватность от этого не слабеет: внешние картинки по-прежнему не грузятся до нажатия «Show images», трекинг-пиксели вырезаются, скрипты не переживают санитайзер. Авторские `style` / `<style>` проходят только через CSS allow-list T-144: типографика, размеры, отступы, таблицы и responsive `@media` остаются; любые `url()`, `@import`, `@font-face`, generated content, fixed-оверлеи и неизвестные конструкции удаляются.
- Спрятанный отправителем преголовок не всплывает наверх письма после сужения `style=` до allow-list — и при этом **письмо не худеет** (T-142). Признаки разведены по смыслу: «коробки нет» (`display:none`, `visibility:hidden`, `opacity:0`, `mso-hide:all`, `max-height:0`+`overflow:hidden`, атрибут `hidden`) убирает поддерево целиком; «текст не прочитать» (`font-size:0`, `color:transparent`) наследуется вниз как в CSS, гасит только текст и сбрасывается на первом потомке со своим размером или цветом. `font-size:0` — это вёрсточная привычка почты (MJML ставит её на каждую обёртку, чтобы схлопнуть пробелы), а не признак тайны.
- Проход не имеет права оставить пустое письмо: скрытое поддерево кончается вместе со своим предком, каким бы несбалансированным ни была вёрстка, а если после прохода не осталось ни текста, ни картинок — читателю отдаётся исходный HTML (санитайзер отработает поверх).

### Incoming attachment row

- Под мета-строкой письма: `paper-recess`, hairline 1px, радиус 12px, padding 8×12px; без тени и без акцентной заливки.
- Слева paperclip 16px `ink-secondary`; имя — Body-sm 15/500 `ink`, вторая строка `MIME · size` — Label 13/500 `ink-secondary`.
- Справа `Open` и `Save as`: тихие text-actions 13px, высота не ниже 32px, `paper-pane` + `stroke-soft`, радиус 12px. Hover = `paper-wash`; disabled = tertiary.

### Thread stack (ветка из нескольких писем)

- Открытое письмо — **сверху**, история ветки под ним, от новых к старым.
  Порядок карточек — порядок ветки, а не порядок кликов: раскрытие
  карточки не переставляет стопку под указателем. Данные при этом
  остаются в хронологии (старое → новое) — переворачивается вид, не факт
  о том, какое письмо последнее.
- Свёрнутая карточка — `paper-recess`, hairline, радиус 12px, padding
  12×16px и **inset 12px с каждой стороны**: история уже открытого
  письма, и разница в ширине — это и есть отличие «стопки заголовков»
  от «письма». Открытая карточка держит полную ширину колонки.
- Внутри свёрнутой — имя отправителя слева (ellipsis) и время справа,
  без превью текста: это оглавление ветки, а не второй список.
- Экран не делится между письмом и всей историей: открытая карточка
  занимает видимую высоту стопки минус место под **две** свёрнутые
  карточки, остальные — за скроллом. Длинное письмо выше этого минимума,
  и тогда прокручивается вся колонка.

### Ссылки в теле письма

- Ссылка выглядит ссылкой и в plain-text письме, не только в HTML: тело
  экранируется целиком и размечается `<a href>` только вокруг того, что
  распознано как `https://`, `http://` или `mailto:`. Никакой другой
  схеме кликабельной не быть — `javascript:` и `file:` в тексте
  остаются текстом.
- Точка предложения после ссылки — часть предложения, а не адреса;
  закрывающая скобка отрезается только если адрес её не открывал.
- Клик — одна дверь на оба тела (plain и санитизированный HTML):
  подтверждение по настройке, отказ и тост общие (D44).

### Reply bar

- Прибит к низу превью и выровнен по левому краю. Обводка `stroke-soft` 1px, радиус 12px, компактная высота 38px, внутри стрелка reply + «Reply to {Name}» tertiary. Не полноценный composer.

### Chips

- Метка папки у subject: recess, 13px, пилюля, secondary text. Не accent.

### Toast (Undo, нет на кадре — наследовать)

- 12px радиус, ink на paper-pane, hairline border, без тени. Action «Undo» accent text, не кнопка-пилюля.

### Empty / skeleton (нет на кадре)

- Текст 15px secondary, никакого иллюстративного 3D. Skeleton = 8px-радиус полоски `paper-recess`, не shimmer-радуга.

### Ожидание вместо замирания (T-133)

- Окно не имеет права замирать ни на кадр. Запись в SQLite на GTK-потоке — не «мелкая операция»: писателей база сериализует, и `busy_timeout` у неё 5 секунд, так что любая такая запись во время фоновой докачки почты стоит ровно этих пяти секунд. Настройки пишутся только через поток-писатель (`App::write_settings`); это правило закрыто тестом, а не соглашением.
- Действие, за которым стоит работа, говорит об этом на месте нажатия и остаётся неактивным, пока работа идёт: «Show images» → «Loading…». Не спиннер поверх уже читаемого письма — читателю не нужно, чтобы у него забирали текст, который уже на экране.
- Скелетон — для того, чего ещё нет. Занятое состояние — для того, что уже показано и обновляется.
- Отметка о прочтении, «отметить всю папку прочитанной» и прочие записи в почтовые таблицы идут через поток-писатель (`crate::mail_writer`), а не с GTK-потока (T-139, T-140). Строка теряет точку непрочитанного в момент нажатия — оптимистично, — а ответ базы перечитывает её и, если запись не прошла, возвращает точку и говорит об этом тостом. Молча потерянная отметка — дефект, а не мелочь.

### Хром окна следует за темой (T-137, T-138)

- Верхняя полоса окна с кнопками управления, скроллбары, подсказки и системные диалоги нарисованы темой GTK, а не `style.css`. Приложение обязано сообщать ей выбранный вариант (`gtk-application-prefer-dark-theme`) вместе с загрузкой своих токенов: тёмная оболочка под светлым заголовком — не «особенность рабочего стола», а несделанная работа.
- Значение самого рабочего стола читается один раз, до первой нашей записи в ту же настройку. Иначе «System» перестаёт означать рабочий стол и начинает означать наш последний выбор.
- Скроллбар — `ink-tertiary`, тот же серый, что у дат; при наведении `ink-secondary`, при перетаскивании `ink`. Жёлоб прозрачный: полоса лежит на панели, а не в прорезанном канале.
- Тема, которую выбрал читатель, действует с первого кадра после запуска. CSS грузится тогда, когда настройки профиля уже прочитаны, а не раньше «на всякий случай».

## Do's and Don'ts

### Do:

- **Do** брать палитру и радиусы из frontmatter. Мокап — суд структуры и цвета; кегль и воздух — D70.
- **Do** держать Compose единственной заливной синей поверхностью на экране (плюс мелкие точки/счётчики).
- **Do** виртуализировать список: стиль строки не имеет права требовать полных DOM-карточек.
- **Do** в GTK мапить токены в CSS variables один-в-один (`--paper-sidebar`, `--accent`, …).

### Don't:

- **Don't** возвращать focus ring — он снят решением T-097(5); возврат роняет `nothing_wears_a_focus_ring_and_nothing_falls_back_to_the_host_theme` (crates/app/src/main.rs).
- **Don't** ставить календарную иконку в тулбар — её нет в продукте MVP, даже если она на кадре. Тулбар: Archive, Delete, Unread, Snooze, Read, Overflow.
- **Don't** тёплый off-white, terracotta, glassmorphism, градиенты, огромные тени, 24px иконки.
- **Don't** Inter Display / serif / mono в Inbox.
- **Don't** обводить выбранную строку, подкладывать карточную тень, скруглять целые колонки.
- **Don't** второй акцент «для папок» в хроме — точки папок остаются 8px.
- **Don't** копировать macOS traffic lights в Linux-окно.
- **Don't** полноэкранный spinner. Пустой кэш = skeleton в списке, сайдбар и хром уже на месте.
