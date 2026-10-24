# Industry packs

A pack is a schema layer for one industry, installed on top of the built-in
`enterprise` schema: object types, link types, interfaces, a glossary, rules,
shapes, metrics and action types, plus a sample data set. onto-intelli ships
ten. Every pack reuses the enterprise types (person, team, project,
knowledge, issue and the rest) instead of redefining them, and every name it
adds is unique across all packs, so packs install side by side.

Two things hold for every pack, and each pack's description says both:

- **Its rules are supplied assumptions.** What they derive is labelled with
  the rule that derived it and is never stored as a fact; a certificate that
  cites them earns `entailed_under_supplied_rules`, never `entailed`
  ([ASSURANCE.md](ASSURANCE.md#built-in-and-supplied-rules)).
- **It borrows vocabulary.** The standards it names are where its words come
  from; a pack claims no compliance with any of them.

Each pack's sample makes every one of its rules fire at least once and holds
exactly one deliberate shape violation, so `onto shapes validate` has
something to show. Every output on this page is from a real run on PostgreSQL;
a `…` stands for lines left out.

## Installing a pack

```text
$ onto pack list
manufacturing  1.0.0    Manufacturing operations                 71 changes to install
    manufacturing · 11 object types, 18 link types, 6 rules, 5 shapes, 3 metrics, 3 actions, 30 sample entities · borrows from ISA-95 / IEC 62264 (enterprise-control system integration): equipment hierarchy and production order vocabulary, ISA-88 / IEC 61512 (batch control): lot and batch vocabulary, ISO 9001 (quality management): nonconformance vocabulary
supply-chain   1.0.0    Supply chain and sourcing                61 changes to install
    supply_chain · 10 object types, 17 link types, 6 rules, 5 shapes, 3 metrics, 3 actions, 34 sample entities · borrows from SCOR (Supply Chain Operations Reference model): source, make and deliver vocabulary, GS1 identification keys: GLN for supplier sites, ISO 28000 (security management for the supply chain): disruption and risk vocabulary
retail         1.0.0    Retail and omnichannel commerce          66 changes to install
    retail · 11 object types, 18 link types, 6 rules, 4 shapes, 3 metrics, 2 actions, 36 sample entities · borrows from GS1 GTIN (Global Trade Item Number): product identification, GS1 Global Product Classification: merchandise hierarchy vocabulary, ARTS Operational Data Model (NRF): store, item and transaction vocabulary
logistics      1.0.0    Logistics and transportation             70 changes to install
    logistics · 11 object types, 19 link types, 6 rules, 4 shapes, 3 metrics, 2 actions, 28 sample entities · borrows from GS1 logistics keys: SSCC for handling units, GLN for locations, UN/LOCODE (UNECE): hub location codes, WCO Harmonized System: commodity codes on customs declarations
healthcare     1.0.0    Healthcare delivery                      69 changes to install
    healthcare · 11 object types, 16 link types, 6 rules, 5 shapes, 4 metrics, 3 actions, 35 sample entities · borrows from vocabulary inspired by HL7 FHIR R4 resources (Patient, Encounter, Condition, Observation, Medication, MedicationRequest, CarePlan, Location), ICD-10 and SNOMED CT code fields (lexical shape only, terminologies not included), LOINC observation code fields (lexical shape only), drug classes in the style of the WHO ATC classification
finance        1.0.0    Banking, KYC and AML                     70 changes to install
    financial_services · 10 object types, 15 link types, 6 rules, 5 shapes, 4 metrics, 3 actions, 41 sample entities · borrows from vocabulary inspired by the EDM Council Financial Industry Business Ontology (FIBO): legal entity, control, account, payment, customer due diligence and beneficial ownership terms in the style of the FATF Recommendations, payment fields in the style of ISO 20022 credit transfers, identifier fields shaped like ISO 17442 (LEI), ISO 13616 (IBAN), ISO 3166-1 alpha-2 and ISO 4217 codes (lexical shape only)
insurance      1.0.0    Property and casualty insurance          66 changes to install
    insurance · 9 object types, 19 link types, 6 rules, 4 shapes, 4 metrics, 3 actions, 37 sample entities · borrows from vocabulary inspired by the ACORD insurance data standards (party, policy, coverage, claim), lines of business in the style of the Solvency II classification, reinsurance treaty types as used in common market practice (quota share, surplus, excess of loss, stop loss)
energy         1.0.0    Energy and utilities                     69 changes to install
    energy · 11 object types, 14 link types, 6 rules, 5 shapes, 4 metrics, 3 actions, 32 sample entities · borrows from vocabulary inspired by the IEC 61968/61970 Common Information Model (CIM), asset-condition terms inspired by ISO 55000 asset management, interruption terms inspired by IEEE 1366 distribution reliability indices
telecom        1.0.0    Telecommunications                       66 changes to install
    telecom · 10 object types, 14 link types, 6 rules, 5 shapes, 4 metrics, 3 actions, 32 sample entities · borrows from resource and service vocabulary inspired by the TM Forum Information Framework (SID), alarm severity terms inspired by ITU-T X.733 alarm reporting, radio access terms inspired by 3GPP (LTE and NR cells, physical cell identity)
saas           1.0.0    SaaS: product, customers, subscriptions and support 66 changes to install
    software · 10 object types, 16 link types, 6 rules, 5 shapes, 4 metrics, 3 actions, 41 sample entities · borrows from organization and offer vocabulary inspired by schema.org (Organization, Offer, SoftwareApplication), support ticket terms inspired by ITIL 4 incident and service request management, feature flag terms inspired by the OpenFeature specification, survey categories borrowed from the Net Promoter Score method (promoter 9-10, passive 7-8, detractor 0-6)
```

`onto pack show <id>` lists everything a pack defines, with the pack's own
lint findings: those of the pack on the packages it requires that those
packages do not already have (for manufacturing, the recursive multi-level
BOM rule):

```text
$ onto pack show manufacturing
Manufacturing operations manufacturing 1.0.0 manufacturing
Discrete and batch manufacturing: factories, production lines and work centers, parts with multi-level bills of material and routings, production orders with lot genealogy, quality inspections and nonconformances, and equipment maintenance. Vocabulary inspired by ISA-95 / IEC 62264, ISA-88 and ISO 9001; it borrows their words and claims no compliance with any of them. Its rules (bill-of-material explosion, suspect lots, at-risk orders) are supplied assumptions: what they derive is labelled with the rule that derived it and is never stored as a fact. The sample contains one deliberate shape violation (see sample_note).
requires enterprise
borrows vocabulary from ISA-95 / IEC 62264 (enterprise-control system integration): equipment hierarchy and production order vocabulary, ISA-88 / IEC 61512 (batch control): lot and batch vocabulary, ISO 9001 (quality management): nonconformance vocabulary
…
Rules (supplied: assumptions, not facts)
  bom_explosion                uses_component = has_bom > bom_component (chain)
  bom_explosion_multilevel     ?a uses_component ?c <= ?a uses_component ?b, ?b uses_component ?c (horn)
  production_crew              production_crew = scheduled_on > crewed_by (chain)
  suspect_lot                  ?lot a suspect_lot <= ?lot lot_from_order ?o, ?o consumed_lot ?input, ?input state quarantined (horn)
  at_risk_production_order     ?o a at_risk_production_order <= ?o a production_order, ?o state released, ?o scheduled_on ?wc, ?wc state down (horn)
  critical_quality_exposure    ?p has_critical_ncr ?n <= ?n a nonconformance, ?n prop.severity critical, ?n raised_against ?lot, ?lot lot_of_part ?p (horn)
…
sample: production_order:po-1004 is planned but not yet scheduled on a line or work center: the one deliberate violation (shape production_order_shape, min_count 1 on scheduled_on). Everything else in the sample conforms.
Lint (the pack's own findings): 0 errors, 0 warnings, 1 info · a measurement
…
```

`onto pack plan <id>` is the schema plan of the install, with the same risk,
consequences and shape delta as any change set
([ASSURANCE.md](ASSURANCE.md#semantic-plans)). Installing on the demo adds only
schema facts, so it is conservative and low risk:

```text
$ onto pack plan manufacturing
non-breaking on schema v1 · 0 entities · 0 links in scope
  · add object type bill_of_material (0 entities, 0 links)
…
  risk low
  consequences: +4 asserted, -0 removed · conservative
      asserted 4
      + #production_line implements #plant_floor_asset
      + #work_center implements #plant_floor_asset
      + after_operation a transitive
      + housed_in a transitive
  shapes: 0 new results, 0 resolved · undetermined → undetermined
```

`onto pack install <id>` applies the change set (roles in
`governance.apply_roles`); `--propose` files it as a schema proposal instead,
and `--sample` loads the sample into the workspace (`-w`) after the schema
change. An agent always proposes. Installing again changes nothing:

```text
$ onto pack install manufacturing
installed manufacturing 1.0.0 already: nothing to change
$ onto pack install telecom --propose --message "network operations want alarms in the ontology"
proposed telecom 1.0.0 66 schema changes · proposal prop_1a16e22845c0000031474 (review with `onto schema proposal prop_1a16e22845c0000031474`)
```

A pack is also a built-in schema package, so a configuration can load it at
startup:

```toml
[ontology]
extend = ["builtin:enterprise", "builtin:manufacturing"]
```

The same operations are the actions `pack.list`, `pack.show`, `pack.plan` and
`pack.install`, the HTTP routes `GET /v1/packs`, `GET /v1/packs/{id}`,
`POST /v1/packs/{id}/plan` and `POST /v1/packs/{id}/install`, the MCP tools
`list_packs`, `show_pack`, `plan_pack` and `install_pack` (a proposal), and the
**Packs** tab of the web UI's Assurance area.

Samples use fixed entity ids, and no two samples share one, so every sample
loads into a workspace of its own in one database: the outputs below come
from installing the packs one after another, each with its sample in its own
workspace (`-w pk-<pack>`).

## The packs

| Pack | Title | Object types | Link types | Rules | Shapes | Metrics | Action types | Sample entities |
|---|---|---|---|---|---|---|---|---|
| [`manufacturing`](#manufacturing-operations-manufacturing) | Manufacturing operations | 11 | 18 | 6 | 5 | 3 | 3 | 30 |
| [`supply-chain`](#supply-chain-and-sourcing-supply-chain) | Supply chain and sourcing | 10 | 17 | 6 | 5 | 3 | 3 | 34 |
| [`retail`](#retail-and-omnichannel-commerce-retail) | Retail and omnichannel commerce | 11 | 18 | 6 | 4 | 3 | 2 | 36 |
| [`logistics`](#logistics-and-transportation-logistics) | Logistics and transportation | 11 | 19 | 6 | 4 | 3 | 2 | 28 |
| [`healthcare`](#healthcare-delivery-healthcare) | Healthcare delivery | 11 | 16 | 6 | 5 | 4 | 3 | 35 |
| [`finance`](#banking-kyc-and-aml-finance) | Banking, KYC and AML | 10 | 15 | 6 | 5 | 4 | 3 | 41 |
| [`insurance`](#property-and-casualty-insurance-insurance) | Property and casualty insurance | 9 | 19 | 6 | 4 | 4 | 3 | 37 |
| [`energy`](#energy-and-utilities-energy) | Energy and utilities | 11 | 14 | 6 | 5 | 4 | 3 | 32 |
| [`telecom`](#telecommunications-telecom) | Telecommunications | 10 | 14 | 6 | 5 | 4 | 3 | 32 |
| [`saas`](#saas-product-customers-subscriptions-and-support-saas) | SaaS: product, customers, subscriptions and support | 10 | 16 | 6 | 5 | 4 | 3 | 41 |

## Manufacturing operations (`manufacturing`)

Plants, lines and work centers; parts with multi-level bills of material and routings; production orders with lot genealogy; quality inspections, nonconformances and maintenance. People and crews are the enterprise `person` and `team`.

11 object types, 1 interface, 18 link types, 6 rules, 5 shapes, 3 metrics, 3 action types, 24 glossary terms and 30 sample entities. Industry `manufacturing`, version 1.0.0, requires `enterprise`.

| Object type | What it is |
|---|---|
| `bill_of_material` | One revision of the list of parts that make up a part (single level; the rules explode it). |
| `factory` | A manufacturing plant (an ISA-95 site): where lines, work centers and inventory live. |
| `inspection_lot` | A quality inspection of a production lot and its result. |
| `maintenance_order` | Preventive, corrective or predictive maintenance work on a work center or line. |
| `nonconformance` | A recorded deviation from specification (an NCR) against a lot, a part or a work center. |
| `part` | A material, component, subassembly or finished good identified by its part number. |
| `production_line` | A line or area inside a factory where an ordered sequence of work centers builds parts. (implements `plant_floor_asset`) |
| `production_lot` | A traceable lot or batch of one part, produced by an order or received from a supplier. |
| `production_order` | An order to make a quantity of one part on a line or work center (a work order or manufacturing order). |
| `routing_operation` | One step of the routing that makes a part: what is done, where, and after which other steps. |
| `work_center` | A machine, cell or workstation where operations are performed (an ISA-95 work unit). (implements `plant_floor_asset`) |

Interfaces: `plant_floor_asset` (Lines and work centers: equipment on the plant floor that is housed somewhere and can stop production).

Link types: `after_operation`, `bom_component`, `bom_for`, `consumed_lot`, `crewed_by`, `housed_in`, `inspects_lot`, `lot_from_order`, `lot_of_part`, `maintenance_assignee`, `maintenance_on`, `makes_part`, `ncr_raised_by`, `performed_at`, `plant_manager`, `raised_against`, `routing_for`, `scheduled_on`. Transitive: `after_operation`, `housed_in`. Dependencies: `after_operation`, `consumed_lot`.

| Rule | Form | Derives | On the sample |
|---|---|---|---|
| `bom_explosion` | chain | `uses_component = has_bom > bom_component`: A part uses every component listed on its bill of material. | 3 |
| `bom_explosion_multilevel` | horn | `?a uses_component ?c <= ?a uses_component ?b, ?b uses_component ?c`: A part uses the components of its components (multi-level explosion). | 1 |
| `production_crew` | chain | `production_crew = scheduled_on > crewed_by`: A production order is handled by the crew of the line or work center it is scheduled on. | 3 |
| `suspect_lot` | horn | `?lot a suspect_lot <= ?lot lot_from_order ?o, ?o consumed_lot ?input, ?input state quarantined`: A lot made by an order that consumed a quarantined input lot is suspect until cleared. | 1 |
| `at_risk_production_order` | horn | `?o a at_risk_production_order <= ?o a production_order, ?o state released, ?o scheduled_on ?wc, ?wc state down`: A released production order scheduled on equipment that is down is at risk. | 1 |
| `critical_quality_exposure` | horn | `?p has_critical_ncr ?n <= ?n a nonconformance, ?n prop.severity critical, ?n raised_against ?lot, ?lot lot_of_part ?p`: A part has a critical nonconformance when one is raised against one of its lots. | 1 |

Derived classes: `at_risk_production_order`, `suspect_lot`. Derived relations: `has_critical_ncr`, `production_crew`, `uses_component`.

| Shape | Target | Checks |
|---|---|---|
| `bom_shape` | `bill_of_material` | A bill of material describes exactly one part and lists at least one component. |
| `nonconformance_shape` | `nonconformance` | A nonconformance says what it was found on and how severe it is. |
| `part_shape` | `part` | Every part carries a well-formed part number. |
| `plant_floor_asset_shape` | `plant_floor_asset` | Lines and work centers sit inside a line or a factory. |
| `production_order_shape` | `production_order` | Every production order makes exactly one part, runs somewhere and has a positive quantity. |

| Metric | Measures |
|---|---|
| `nonconformance_lead_time` | lead_time over `nonconformance`: Days from a nonconformance being raised to being closed. |
| `open_production_orders` | count over `production_order`: Open production orders by line, part, state and priority. By crew, line, part, priority, state. Certified. |
| `scheduled_production_quantity` | sum over `production_order`: Units still to make on open production orders. By line, part. |

| Action type | Risk | Roles | Does |
|---|---|---|---|
| `quarantine_lot` | medium | any writer | Put a production lot on quality hold. |
| `raise_nonconformance` | low | any writer | Record a nonconformance against a lot, a part or a work center. |
| `release_production_order` | low | member, admin | Release a planned production order to a line or work center. |

**The deliberate violation.** production_order:po-1004 is planned but not yet scheduled on a line or work center: the one deliberate violation (shape production_order_shape, min_count 1 on scheduled_on). Everything else in the sample conforms. Installed with its sample into the demo workspace:

```text
$ onto pack install manufacturing --sample
installed manufacturing 1.0.0 71 schema changes · schema v2
  sample: 30 entities, 44 links
$ onto reason run
62 facts derived from 345 asserted (16 rules · 3 rounds · fixpoint reached)
…
$ onto shapes validate
violates 15 focus nodes · 1 violations, 0 warnings, 0 info
  severity  shape                    component    focus                    path               message
  violation production_order_shape   min_count    production_order:po-1004 scheduled_on       Schedule the order on a line or work center.
…
```

**Standards it borrows vocabulary from** (no compliance claimed):

- ISA-95 / IEC 62264 (enterprise-control system integration): equipment hierarchy and production order vocabulary
- ISA-88 / IEC 61512 (batch control): lot and batch vocabulary
- ISO 9001 (quality management): nonconformance vocabulary

## Supply chain and sourcing (`supply-chain`)

A supplier network across tiers: suppliers and their sites, sourcing regions with a trade-compliance status, purchased items and categories, purchase orders, contracts and receipts, audits and disruptions. Its rules carry sanctions and disruption risk up the network.

10 object types, 1 interface, 17 link types, 6 rules, 5 shapes, 3 metrics, 3 action types, 16 glossary terms and 34 sample entities. Industry `supply_chain`, version 1.0.0, requires `enterprise`.

| Object type | What it is |
|---|---|
| `goods_receipt` | A recorded delivery of goods against a purchase order. |
| `purchase_order` | A commitment to buy items from one supplier. |
| `purchased_item` | A material, component or service bought from suppliers, identified by its item code. |
| `sourcing_category` | A group of purchased items managed with one sourcing strategy (bearings, castings, packaging). |
| `sourcing_contract` | A frame agreement with a supplier: prices, delivery terms and validity. |
| `sourcing_region` | A country, subdivision or economic zone suppliers operate in, with its trade-compliance status. |
| `supplier` | A company that sells goods or services to the organization, at any tier of the supply network. (implements `supply_risk_rated`) |
| `supplier_audit` | An assessment of a supplier or site: quality, social, environmental or security. |
| `supplier_site` | A physical facility of a supplier: a factory, warehouse, mine, farm or office. (implements `supply_risk_rated`) |
| `supply_disruption` | An event that interrupts supply: a disaster, a strike, a port closure, a cyber attack. |

Interfaces: `supply_risk_rated` (Suppliers and supplier sites: parties that carry a sourcing risk rating).

Link types: `audit_of`, `category_managed_by`, `contract_with`, `disruption_owner`, `disrupts`, `in_sourcing_category`, `operates_site`, `ordered_from`, `orders_item`, `po_buyer`, `produced_at_site`, `receipt_for`, `region_within`, `site_in_region`, `sources_from`, `supplies_item`, `under_contract`. Transitive: `region_within`. Dependencies: `sources_from`.

| Rule | Form | Derives | On the sample |
|---|---|---|---|
| `sanctioned_site` | horn | `?site a sanctioned_site <= ?site a supplier_site, ?site site_in_region ?r, ?r state sanctioned`: A supplier site located in a sanctioned region is a sanctioned site. | 1 |
| `sanctioned_site_within` | horn | `?site a sanctioned_site <= ?site site_in_region ?r, ?r region_within ?c, ?c state sanctioned`: A supplier site in a region that lies inside a sanctioned region is a sanctioned site. | 1 |
| `high_risk_supplier` | horn | `?s a high_risk_supplier <= ?s a supplier, ?s operates_site ?site, ?site a sanctioned_site`: A supplier operating a sanctioned site is high risk. | 2 |
| `upstream_supply_risk` | horn | `?s inherits_supply_risk_from ?t <= ?s sources_from ?t, ?t a high_risk_supplier`: A supplier that buys directly from a high-risk supplier inherits its risk (the first tier). | 2 |
| `upstream_supply_risk_tiers` | chain | `inherits_supply_risk_from = sources_from > inherits_supply_risk_from`: A supplier that buys directly from a supplier that inherits a risk inherits it too, so the risk climbs the network one tier per step (tier-n reach is this assumption, not a property of sources_from). | 1 |
| `order_disruption_exposure` | horn | `?po at_risk_from_disruption ?d <= ?d a supply_disruption, ?d prop.impact_level high, ?d disrupts ?site, ?s operates_site ?site, ?po ordered_from ?s`: A purchase order placed with a supplier whose site is hit by a high-impact disruption is at risk from it. | 1 |

Derived classes: `high_risk_supplier`, `sanctioned_site`. Derived relations: `at_risk_from_disruption`, `inherits_supply_risk_from`.

| Shape | Target | Checks |
|---|---|---|
| `purchase_order_shape` | `purchase_order` | A purchase order is placed with exactly one supplier, lists items and has a non-negative amount. |
| `sourcing_contract_shape` | `sourcing_contract` | A contract names exactly one supplier and an end of validity. |
| `supplier_audit_shape` | `supplier_audit` | An audit assesses exactly one supplier or site and scores it from 0 to 100. |
| `supplier_shape` | `supplier` | A supplier has at least one site on record (so its region can be assessed) and a well-formed D-U-N-S number. |
| `supplier_site_shape` | `supplier_site` | A supplier site sits in exactly one sourcing region. |

| Metric | Measures |
|---|---|
| `disruption_resolution_time` | lead_time over `supply_disruption`: Days from a supply disruption being reported to being resolved. |
| `open_purchase_orders` | count over `purchase_order`: Open purchase orders by state and supplier. By state, supplier. |
| `open_purchase_spend` | sum over `purchase_order`: Committed spend on open purchase orders. By buyer, category, currency, supplier. Certified. |

| Action type | Risk | Roles | Does |
|---|---|---|---|
| `issue_purchase_order` | low | member, admin | Issue a draft purchase order to its supplier. |
| `record_supply_disruption` | low | any writer | Record a supply disruption hitting a supplier site. |
| `suspend_supplier` | high | admin | Suspend a supplier: no new purchase orders until it is reinstated. |

**The deliberate violation.** supplier:northwind-distribution has no site on record: the one deliberate violation (shape supplier_shape, min_count 1 on operates_site). Everything else in the sample conforms. Ruritania (XR) and its regions are fictional. Installed with its sample into a workspace of its own:

```text
$ onto -w pk-supply-chain pack install supply-chain --sample
installed supply-chain 1.0.0 61 schema changes · schema v4
  sample: 34 entities, 43 links
$ onto -w pk-supply-chain reason run --samples 0
20 facts derived from 121 asserted (22 rules · 6 rounds · fixpoint reached)
…
$ onto -w pk-supply-chain shapes validate
violates 17 focus nodes · 1 violations, 0 warnings, 0 info
  severity  shape                    component    focus                    path               message
  violation supplier_shape           min_count    supplier:northwind-distribution operates_site      Record at least one site so the supplier's sourcing region can be assessed.
…
```

**Standards it borrows vocabulary from** (no compliance claimed):

- SCOR (Supply Chain Operations Reference model): source, make and deliver vocabulary
- GS1 identification keys: GLN for supplier sites
- ISO 28000 (security management for the supply chain): disruption and risk vocabulary

## Retail and omnichannel commerce (`retail`)

Store and online retail: stores in nested districts, SKUs in a merchandise hierarchy, stock positions and planograms per store, shoppers, orders and returns, promotions and recalls. Its rules work out what a promotion or a recall reaches through the category hierarchy.

11 object types, 1 interface, 18 link types, 6 rules, 4 shapes, 3 metrics, 2 action types, 21 glossary terms and 36 sample entities. Industry `retail`, version 1.0.0, requires `enterprise`.

| Object type | What it is |
|---|---|
| `merch_category` | A node of the merchandise hierarchy (department, category, subcategory). |
| `planogram` | The shelf layout of a merchandise category in a store. (implements `store_scoped`) |
| `product_recall` | A notice to pull SKUs from sale and contact the shoppers who bought them. |
| `product_return` | Goods a shopper brings or sends back, and the refund decision. |
| `promotion` | A time-boxed price or bundle offer on SKUs or whole categories. |
| `retail_district` | A group of stores managed together; districts nest into regions. |
| `retail_order` | A sale to a shopper, in a store or through a digital channel. |
| `retail_store` | A physical or online store that sells to shoppers and holds stock. |
| `shopper` | A known shopper, usually a loyalty program member. |
| `sku` | A stock keeping unit: one sellable product variant, identified by its GTIN. |
| `stock_position` | How much of one SKU a store holds, and whether it needs replenishing. (implements `store_scoped`) |

Interfaces: `store_scoped` (Records kept per store: stock positions and planograms).

Link types: `at_store`, `district_within`, `fulfilled_from`, `in_merch_category`, `merch_owner`, `merch_subcategory_of`, `order_contains`, `placed_by_shopper`, `planogram_for`, `promotes`, `promotion_runs_at`, `recall_managed_by`, `recalls_sku`, `return_of`, `returned_sku`, `stock_of`, `store_in_district`, `store_managed_by`. Transitive: `district_within`, `merch_subcategory_of`.

| Rule | Form | Derives | On the sample |
|---|---|---|---|
| `promotion_covers_sku` | horn | `?promo covers_sku ?sku <= ?promo promotes ?sku, ?sku a sku`: A promotion on a SKU covers that SKU. | 1 |
| `promotion_covers_category` | chain | `covers_sku = promotes > category_skus`: A promotion on a category covers every SKU assigned to that category. | 4 |
| `promotion_covers_subcategories` | chain | `covers_sku = promotes > merch_subcategories > category_skus`: A promotion on a category also covers every SKU of the categories beneath it, at any depth of the merchandise hierarchy. | 6 |
| `promoted_stockout` | horn | `?p a promoted_stockout <= ?p a stock_position, ?p prop.stock_status out_of_stock, ?p stock_of ?sku, ?promo covers_sku ?sku, ?promo state live`: A stock position that is out of stock while a live promotion covers its SKU (directly or through its category) loses sales. | 1 |
| `recalled_stock_on_hand` | horn | `?store holds_recalled_sku ?sku <= ?r a product_recall, ?r recalls_sku ?sku, ?p stock_of ?sku, ?p at_store ?store`: A store holding a stock position of a recalled SKU must pull it from the shelf. | 2 |
| `shopper_affected_by_recall` | horn | `?c affected_by_recall ?r <= ?r recalls_sku ?sku, ?o order_contains ?sku, ?o placed_by_shopper ?c`: A shopper who ordered a recalled SKU is affected by the recall and should be contacted. | 1 |

Derived classes: `promoted_stockout`. Derived relations: `affected_by_recall`, `covers_sku`, `holds_recalled_sku`.

| Shape | Target | Checks |
|---|---|---|
| `promotion_shape` | `promotion` | A promotion applies to something and discounts at most 90 percent. |
| `retail_order_shape` | `retail_order` | An order is fulfilled from a store, sells at least one SKU and has a non-negative total. |
| `sku_shape` | `sku` | A SKU has a well-formed GTIN, exactly one merchandise category and a non-negative price. |
| `stock_position_shape` | `stock_position` | A stock position counts one SKU in one store and is never negative. |

| Metric | Measures |
|---|---|
| `open_retail_orders` | count over `retail_order`: Orders not yet fulfilled, returned or cancelled. By channel, district, state, store. Certified. |
| `retail_order_value` | sum over `retail_order`: Order value by channel and store. By channel, store. |
| `return_refunds` | sum over `product_return`: Refunds paid on returns, by reason. By reason. |

| Action type | Risk | Roles | Does |
|---|---|---|---|
| `launch_promotion` | low | member, admin | Take a scheduled promotion live. |
| `pull_recalled_sku` | high | member, admin | Mark a SKU as recalled and attach it to the recall that pulls it from sale. |

**The deliberate violation.** stock_position:0217-cold-brew shows on_hand -3 (a shrink adjustment posted twice): the one deliberate violation (shape stock_position_shape, min_value 0 on prop.on_hand). Everything else in the sample conforms. Installed with its sample into a workspace of its own:

```text
$ onto -w pk-retail pack install retail --sample
installed retail 1.0.0 66 schema changes · schema v5
  sample: 36 entities, 55 links
$ onto -w pk-retail reason run --samples 0
23 facts derived from 136 asserted (28 rules · 3 rounds · fixpoint reached)
…
$ onto -w pk-retail shapes validate
violates 18 focus nodes · 1 violations, 0 warnings, 0 info
  severity  shape                    component    focus                    path               message
  violation stock_position_shape     min_value    stock_position:0217-cold-brew prop.on_hand       On-hand stock cannot be negative: look for double-posted adjustments. (value -3)
…
```

**Standards it borrows vocabulary from** (no compliance claimed):

- GS1 GTIN (Global Trade Item Number): product identification
- GS1 Global Product Classification: merchandise hierarchy vocabulary
- ARTS Operational Data Model (NRF): store, item and transaction vocabulary

## Logistics and transportation (`logistics`)

Freight and parcel movements: carriers and vehicles, hubs, lanes and consignees, shipments split into legs, nested handling units, delivery exceptions, freight invoices and customs declarations.

11 object types, 1 interface, 19 link types, 6 rules, 4 shapes, 3 metrics, 2 action types, 24 glossary terms and 28 sample entities. Industry `logistics`, version 1.0.0, requires `enterprise`.

| Object type | What it is |
|---|---|
| `carrier` | A company that moves freight: a trucking firm, shipping line, airline, railway or parcel network. |
| `consignee` | The party a shipment is delivered to. |
| `customs_declaration` | The declaration that clears a shipment's goods through customs. |
| `delivery_exception` | Something went wrong with a shipment or a leg: damage, loss, delay, refusal, a customs hold. |
| `fleet_vehicle` | A vehicle, vessel, aircraft or railcar a carrier moves freight with. |
| `freight_invoice` | A carrier's bill for moving shipments. |
| `handling_unit` | A parcel, pallet, tote or container, identified by its SSCC; units nest inside each other. |
| `logistics_hub` | A place freight passes through: a warehouse, distribution center, cross-dock, port, airport or rail terminal. |
| `shipment` | Goods moving from a hub to a consignee or another hub under one tracking number. (implements `trackable_movement`) |
| `transport_lane` | A recurring origin-destination pair between two hubs, with its standard transit time. |
| `transport_leg` | One movement of a shipment between two hubs, on one vehicle. (implements `trackable_movement`) |

Interfaces: `trackable_movement` (Shipments and transport legs: movements with an estimated arrival, carried by a carrier).

Link types: `after_leg`, `arrives_at_hub`, `carried_by`, `declares`, `departs_from_hub`, `driven_by`, `exception_on`, `hub_operated_by`, `invoiced_by`, `invoices_shipment`, `lane_destination`, `lane_origin`, `leg_of`, `moves_on_lane`, `on_vehicle`, `packed_in`, `ships_from`, `ships_to`, `vehicle_operated_by`. Transitive: `after_leg`, `packed_in`. Dependencies: `after_leg`.

| Rule | Form | Derives | On the sample |
|---|---|---|---|
| `carrier_via_legs` | chain | `moved_by_carrier = legs > carried_by`: A shipment is moved by every carrier that carries one of its legs. | 2 |
| `leg_dispatch_team` | chain | `dispatched_by_team = departs_from_hub > hub_operated_by`: A leg is dispatched by the team that runs the hub it departs from. | 4 |
| `delayed_shipment` | horn | `?s a delayed_shipment <= ?s a shipment, ?l leg_of ?s, ?l state delayed`: A shipment with a delayed leg is a delayed shipment. | 1 |
| `hub_congestion_risk` | horn | `?s a congestion_risk_shipment <= ?s state in_transit, ?l leg_of ?s, ?l state planned, ?l arrives_at_hub ?h, ?h state congested`: A shipment in transit whose next planned leg arrives at a congested hub is at risk of delay. | 1 |
| `customs_hold` | horn | `?s held_by_customs ?d <= ?d a customs_declaration, ?d state held, ?d declares ?s`: A shipment whose customs declaration is held is held by customs. | 1 |
| `hazmat_vehicle` | horn | `?v a hazmat_vehicle <= ?l on_vehicle ?v, ?l leg_of ?s, ?s prop.hazmat true`: A vehicle that performs a leg of a dangerous-goods shipment needs hazmat certification. | 1 |

Derived classes: `congestion_risk_shipment`, `delayed_shipment`, `hazmat_vehicle`. Derived relations: `dispatched_by_team`, `held_by_customs`, `moved_by_carrier`.

| Shape | Target | Checks |
|---|---|---|
| `customs_declaration_shape` | `customs_declaration` | A customs declaration covers exactly one shipment and carries an HS code. |
| `handling_unit_shape` | `handling_unit` | A handling unit carries an 18-digit SSCC and is packed in something. |
| `shipment_shape` | `shipment` | A shipment has a tracking number, an origin, a destination, at most one carrier and a non-negative weight. |
| `transport_leg_shape` | `transport_leg` | A leg moves exactly one shipment from one hub to another. |

| Metric | Measures |
|---|---|
| `exception_resolution_time` | lead_time over `delivery_exception`: Days from a delivery exception being opened to being resolved. |
| `open_shipment_weight` | sum over `shipment`: Kilograms on open shipments. By carrier, origin. |
| `open_shipments` | count over `shipment`: Shipments not yet delivered, returned or cancelled. By carrier, lane, origin, service_level, state. Certified. |

| Action type | Risk | Roles | Does |
|---|---|---|---|
| `confirm_delivery` | low | any writer | Confirm that a shipment was delivered. |
| `report_delivery_exception` | low | member, admin, agent | Open a delivery exception on a shipment and flag the shipment. |

**The deliberate violation.** handling_unit:pal-2 carries an 11-digit SSCC (an SSCC has 18 digits): the one deliberate violation (shape handling_unit_shape, pattern on prop.sscc). Everything else in the sample conforms. Installed with its sample into a workspace of its own:

```text
$ onto -w pk-logistics pack install logistics --sample
installed logistics 1.0.0 70 schema changes · schema v6
  sample: 28 entities, 51 links
$ onto -w pk-logistics reason run --samples 0
19 facts derived from 124 asserted (34 rules · 2 rounds · fixpoint reached)
…
$ onto -w pk-logistics shapes validate
violates 12 focus nodes · 1 violations, 0 warnings, 0 info
  severity  shape                    component    focus                    path               message
  violation handling_unit_shape      pattern      handling_unit:pal-2      prop.sscc          An SSCC has exactly 18 digits. (value 38712345000)
…
```

**Standards it borrows vocabulary from** (no compliance claimed):

- GS1 logistics keys: SSCC for handling units, GLN for locations
- UN/LOCODE (UNECE): hub location codes
- WCO Harmonized System: commodity codes on customs declarations

## Healthcare delivery (`healthcare`)

Patients, facilities and nested care units, encounters, diagnoses, observations, medications in a drug-class hierarchy, medication orders and care plans. The rules flag (drug-class allergies, critical results); clinical decisions stay with clinicians.

11 object types, 1 interface, 16 link types, 6 rules, 5 shapes, 4 metrics, 3 action types, 23 glossary terms and 35 sample entities. Industry `healthcare`, version 1.0.0, requires `enterprise`.

| Object type | What it is |
|---|---|
| `care_facility` | Hospital, clinic, laboratory or pharmacy that delivers care. |
| `care_plan` | Intended care for a patient: goals and planned activities for one or more diagnoses. (implements `clinical_record`) |
| `care_unit` | Ward, department or bay inside a facility or inside another unit. |
| `clinical_observation` | Measurement or finding: a laboratory result, a vital sign, a score. (implements `clinical_record`) |
| `clinician` | Licensed practitioner (physician, nurse, pharmacist) who provides or orders care. (extends `person`) |
| `diagnosis` | Clinical condition, problem or diagnosis recorded for a patient. (implements `clinical_record`) |
| `drug_class` | Therapeutic or chemical group of medications (for example penicillins). Classes nest. |
| `encounter` | Interaction between a patient and care providers: a visit, an admission, a teleconsultation. (implements `clinical_record`) |
| `medication` | Drug product that can be ordered and administered. |
| `medication_order` | Prescription or inpatient order of a medication for a patient. (implements `clinical_record`) |
| `patient` | Person receiving care. Identified by a medical record number (MRN). |

Interfaces: `clinical_record` (Anything recorded in a patient's chart: encounters, diagnoses, observations, orders and care plans).

Link types: `allergic_to`, `attended_by`, `care_unit_within`, `diagnosed_in`, `drug_class_within`, `follows_guideline`, `for_patient`, `in_drug_class`, `observed_during`, `orders_medication`, `plan_addresses`, `practices_at`, `prescribed_by`, `primary_clinician`, `seen_at`, `supported_by_observation`. Transitive: `care_unit_within`, `drug_class_within`.

| Rule | Form | Derives | On the sample |
|---|---|---|---|
| `treating_clinician` | chain | `treated_by_clinician = patient_records > attended_by`: A clinician attending one of the patient's encounters is treating the patient. | 3 |
| `medication_exposure` | horn | `?p receives_medication ?m <= ?o for_patient ?p, ?o orders_medication ?m, ?o state active`: A patient receives every medication on one of their active orders (draft, on-hold, completed, stopped and cancelled orders do not count). | 3 |
| `drug_class_membership` | chain | `in_drug_class = in_drug_class > drug_class_within`: A medication in a drug class is also in every broader class that class sits within. | 2 |
| `allergy_contraindication` | horn | `?o a contraindicated_order <= ?o orders_medication ?m, ?m in_drug_class ?c, ?o for_patient ?p, ?p allergic_to ?c, ?o state active`: An active order of a medication from a drug class the patient is allergic to is contraindicated. | 1 |
| `critical_result` | horn | `?p a critical_result_patient <= ?r a clinical_observation, ?r prop.interpretation critical, ?r state final, ?r for_patient ?p`: A patient with a final observation interpreted as critical has a critical result. | 1 |
| `critical_result_notification` | horn | `?c notify_critical_result ?p <= ?p a critical_result_patient, ?e for_patient ?p, ?e a encounter, ?e state in_progress, ?e attended_by ?c`: The clinician attending an in-progress encounter of a patient with a critical result must be notified. | 1 |

Derived classes: `contraindicated_order`, `critical_result_patient`. Derived relations: `notify_critical_result`, `receives_medication`, `treated_by_clinician`.

| Shape | Target | Checks |
|---|---|---|
| `diagnosis_coded` | `diagnosis` | Diagnoses carry an ICD-10 code and belong to a patient. |
| `encounter_staffed` | `encounter` | Encounters happen somewhere and someone is responsible for them. |
| `medication_order_complete` | `medication_order` | An order names exactly one medication, its patient and the clinician who signed it. |
| `observation_coded` | `clinical_observation` | Observations carry a LOINC code; measured values are numbers. |
| `patient_identity` | `patient` | Every patient carries one well-formed MRN and a birth date. |

| Metric | Measures |
|---|---|
| `active_inpatients` | count over `encounter`: Inpatient encounters in progress, by care unit. By attending, unit. Certified. |
| `critical_observations` | count over `clinical_observation`: Observations interpreted as critical, by LOINC code. By code, patient. |
| `encounter_length` | lead_time over `encounter`: Days from the encounter record being created to it finishing or being cancelled (record creation time, not recorded_at or admitted_at). |
| `open_medication_orders` | count over `medication_order`: Medication orders not yet completed, cancelled or stopped. By prescriber, route, state. |

| Action type | Risk | Roles | Does |
|---|---|---|---|
| `assign_primary_clinician` | low | any writer | Make a clinician the patient's primary care clinician (replaces the current one). |
| `discharge_patient` | medium | member, admin | Finish an encounter that is in progress or on leave, with a discharge note. |
| `hold_medication_order` | low | any writer | Put an active medication order on hold, recording why. |

**The deliberate violation.** medication_order:mo-1003 is a verbal order with no prescriber recorded yet: the one deliberate violation (shape medication_order_complete, constraint medication_order_complete/prescriber, min_count 1 on prescribed_by). Everything else in the sample conforms. Installed with its sample into a workspace of its own:

```text
$ onto -w pk-healthcare pack install healthcare --sample
installed healthcare 1.0.0 69 schema changes · schema v7
  sample: 35 entities, 55 links
$ onto -w pk-healthcare reason run --samples 0
32 facts derived from 150 asserted (40 rules · 3 rounds · fixpoint reached)
…
$ onto -w pk-healthcare shapes validate
violates 17 focus nodes · 1 violations, 0 warnings, 0 info
  severity  shape                    component    focus                    path               message
  violation medication_order_complete min_count    medication_order:mo-1003 prescribed_by      Orders must be signed by a clinician.
…
```

**Standards it borrows vocabulary from** (no compliance claimed):

- vocabulary inspired by HL7 FHIR R4 resources (Patient, Encounter, Condition, Observation, Medication, MedicationRequest, CarePlan, Location)
- ICD-10 and SNOMED CT code fields (lexical shape only, terminologies not included)
- LOINC observation code fields (lexical shape only)
- drug classes in the style of the WHO ATC classification

## Banking, KYC and AML (`finance`)

Banking customers (legal entities and private individuals), jurisdictions, accounts, products, funds transfers, KYC reviews, AML alerts, suspicious activity reports and sanctions designations. The rules flag parties and transfers for review; they are not legal determinations.

10 object types, 1 interface, 15 link types, 6 rules, 5 shapes, 4 metrics, 3 action types, 26 glossary terms and 41 sample entities. Industry `financial_services`, version 1.0.0, requires `enterprise`.

| Object type | What it is |
|---|---|
| `aml_alert` | Transaction-monitoring or screening alert that an analyst must disposition. |
| `bank_account` | Account at this bank or, for counterparties, at another institution. |
| `banking_product` | Product an account is opened under: a current account, a loan, a custody service. |
| `funds_transfer` | Payment from one account to another (credit transfer, card payment, cash movement). |
| `jurisdiction` | Country, territory or sub-national region with its own law, used for residence and risk. |
| `kyc_review` | Customer due diligence case: onboarding or periodic review of a party. |
| `legal_entity` | Company, partnership, trust, fund or other organization that is (or is connected to) a customer. (implements `customer_party`) |
| `private_individual` | Natural person who is a customer, a beneficial owner or a connected party (not a workspace member). (implements `customer_party`) |
| `sanctions_designation` | Entry on a sanctions list naming a person, an entity or a vessel. |
| `suspicious_activity_report` | Report filed with the financial intelligence unit (SAR or STR). |

Interfaces: `customer_party` (A party the bank knows: legal entities and private individuals).

Link types: `account_holder`, `alert_subject`, `connected_party`, `controls_entity`, `credits_account`, `debits_account`, `designation_jurisdiction`, `investigated_by`, `matches_designation`, `relationship_manager`, `reported_in_sar`, `resident_in`, `reviews_party`, `sub_jurisdiction_of`, `under_product`. Transitive: `controls_entity`, `sub_jurisdiction_of`. Symmetric: `connected_party`.

| Rule | Form | Derives | On the sample |
|---|---|---|---|
| `transfer_counterparty` | chain | `pays_party = holds_account > debited_by_transfer > credits_account > account_holder`: A party paying from its account into another party's account pays that party. | 5 |
| `residence_rollup` | chain | `resident_in = resident_in > sub_jurisdiction_of`: A party resident in a region is resident in every jurisdiction the region lies within. | 3 |
| `sanctions_match` | horn | `?p a sanctioned_party <= ?p matches_designation ?d, ?d state active`: A party matched to an active sanctions designation is a sanctioned party. | 1 |
| `sanctioned_by_control` | horn | `?e a sanctioned_party <= ?c a sanctioned_party, ?c controls_entity ?e`: An entity controlled by a sanctioned party is treated as sanctioned (in the spirit of ownership-and-control rules). | 2 |
| `sanctioned_beneficiary` | horn | `?t pays_sanctioned_party ?p <= ?t credits_account ?acct, ?acct account_holder ?p, ?p a sanctioned_party`: A transfer credited to an account held by a sanctioned party pays that party. | 2 |
| `edd_high_risk_jurisdiction` | horn | `?p a enhanced_due_diligence_party <= ?p a customer_party, ?p resident_in ?j, ?j prop.fatf_list call_for_action`: A party resident in a jurisdiction under a FATF call for action needs enhanced due diligence. | 2 |

Derived classes: `enhanced_due_diligence_party`, `sanctioned_party`. Derived relations: `pays_party`, `pays_sanctioned_party`.

| Shape | Target | Checks |
|---|---|---|
| `bank_account_held` | `bank_account` | Every account has a holder and a currency. |
| `funds_transfer_complete` | `funds_transfer` | A transfer moves a positive amount from exactly one account to exactly one account. |
| `kyc_review_scoped` | `kyc_review` | A KYC review names one party and a due diligence level. |
| `legal_entity_identified` | `legal_entity` | Legal entities carry at most one well-formed LEI, a country of incorporation and a residence. |
| `sar_grounded` | `suspicious_activity_report` | A suspicious activity report is grounded in at least one alert and carries a narrative. |

| Metric | Measures |
|---|---|
| `held_transfer_value` | sum over `funds_transfer`: Value of transfers held for review, by currency and channel. By channel, currency. |
| `kyc_customers_by_risk` | count over `customer_party`: Customers by risk rating and PEP status. By kind, pep, risk. |
| `kyc_review_cycle_time` | lead_time over `kyc_review`: Days from a KYC review being opened to its approval or rejection. |
| `open_aml_alerts` | count over `aml_alert`: AML alerts not yet closed, by scenario and analyst. By analyst, scenario, state. Certified. |

| Action type | Risk | Roles | Does |
|---|---|---|---|
| `escalate_aml_alert` | medium | any writer | Escalate an AML alert to an analyst for investigation. |
| `file_sar` | high | compliance_officer, admin | File a suspicious activity report for an escalated alert with the financial intelligence unit and close the alert as reported. |
| `freeze_bank_account` | high | compliance_officer, admin | Freeze an account pending investigation, recording the reason. |

**The deliberate violation.** legal_entity:northwind-holdings carries a 19-character LEI (an LEI has 20): the one deliberate violation (shape legal_entity_identified, constraint legal_entity_identified/lei, pattern on prop.lei). Everything else in the sample conforms. Installed with its sample into a workspace of its own:

```text
$ onto -w pk-finance pack install finance --sample
installed finance 1.0.0 70 schema changes · schema v8
  sample: 41 entities, 53 links
$ onto -w pk-finance reason run --samples 0
26 facts derived from 167 asserted (46 rules · 4 rounds · fixpoint reached)
…
$ onto -w pk-finance shapes validate
violates 19 focus nodes · 1 violations, 0 warnings, 0 info
  severity  shape                    component    focus                    path               message
  violation legal_entity_identified  pattern      legal_entity:northwind-holdings prop.lei           An LEI is 18 alphanumerics followed by 2 check digits. (value 213800ABCDEF1234567)
…
```

**Standards it borrows vocabulary from** (no compliance claimed):

- vocabulary inspired by the EDM Council Financial Industry Business Ontology (FIBO): legal entity, control, account, payment
- customer due diligence and beneficial ownership terms in the style of the FATF Recommendations
- payment fields in the style of ISO 20022 credit transfers
- identifier fields shaped like ISO 17442 (LEI), ISO 13616 (IBAN), ISO 3166-1 alpha-2 and ISO 4217 codes (lexical shape only)

## Property and casualty insurance (`insurance`)

Property and casualty insurance: policy holders, policies and coverage sections, insured items, a peril taxonomy, loss events, claims, intermediaries and reinsurance treaties. The rules flag claims for review; they do not decide coverage.

9 object types, 0 interfaces, 19 link types, 6 rules, 4 shapes, 4 metrics, 3 action types, 21 glossary terms and 37 sample entities. Industry `insurance`, version 1.0.0, requires `enterprise`.

| Object type | What it is |
|---|---|
| `claim` | Request by a policy holder for payment under a policy after a loss. |
| `coverage_section` | Section of a policy: what is covered, up to which limit, after which deductible. |
| `insurance_policy` | Contract of insurance between the insurer and a policy holder for a period. |
| `insured_item` | Vehicle, building, contents, vessel or other object a policy insures. |
| `intermediary` | Broker, agent or aggregator that places policies with the insurer. |
| `loss_event` | Occurrence that causes many claims at once, such as a named storm or a regional flood. |
| `peril` | Cause of loss (fire, flood, storm surge, theft, cyber attack). Perils form a taxonomy. |
| `policy_holder` | Person or business that holds one or more insurance policies. |
| `reinsurance_treaty` | Agreement ceding part of a portfolio's risk to reinsurers. |

Link types: `adjusted_by`, `arises_from_event`, `caused_by_peril`, `ceded_to`, `claim_filed_by`, `claim_under`, `concerns_item`, `covers_peril`, `event_peril`, `excludes_peril`, `exposed_to_peril`, `insures_item`, `invokes_coverage`, `linked_claim`, `placed_through`, `policy_issued_to`, `section_of_policy`, `sub_peril_of`, `underwritten_by`. Transitive: `sub_peril_of`. Symmetric: `linked_claim`.

| Rule | Form | Derives | On the sample |
|---|---|---|---|
| `policy_cover_wording` | chain | `peril_in_cover_wording = coverage_sections > covers_peril`: A policy's cover wording names every peril one of its coverage sections covers (exclusions not applied: this does not decide coverage). | 9 |
| `excluded_peril_claim` | horn | `?c a excluded_peril_claim <= ?c invokes_coverage ?s, ?c caused_by_peril ?p, ?s excludes_peril ?p`: A claim whose cause is a peril excluded by the section it invokes needs a coverage review. | 1 |
| `excluded_broader_peril_claim` | horn | `?c a excluded_peril_claim <= ?c invokes_coverage ?s, ?c caused_by_peril ?p, ?p sub_peril_of ?q, ?s excludes_peril ?q`: A claim whose cause is a narrower kind of a peril excluded by the section it invokes needs a coverage review. | 1 |
| `claim_on_lapsed_policy` | horn | `?c a claim_on_lapsed_policy <= ?c claim_under ?pol, ?pol state lapsed`: A claim made under a lapsed policy needs a coverage review. | 1 |
| `duplicate_item_claim` | horn | `?c a duplicate_item_claim <= ?c linked_claim ?d, ?c concerns_item ?i, ?d concerns_item ?i`: Two linked claims on the same insured item may be a duplicate or an opportunistic claim. | 2 |
| `treaty_recovery` | horn | `?e recoverable_under ?t <= ?c arises_from_event ?e, ?c claim_under ?pol, ?pol ceded_to ?t, ?t state active`: A loss event with claims under a policy ceded to a treaty is recoverable under that treaty. | 1 |

Derived classes: `claim_on_lapsed_policy`, `duplicate_item_claim`, `excluded_peril_claim`. Derived relations: `peril_in_cover_wording`, `recoverable_under`.

| Shape | Target | Checks |
|---|---|---|
| `claim_complete` | `claim` | A claim has a well-formed number, a loss date and exactly one policy. |
| `coverage_section_terms` | `coverage_section` | A coverage section belongs to one policy and carries a non-negative limit and deductible. |
| `insurance_policy_complete` | `insurance_policy` | A policy has a well-formed number, a holder, at least one coverage section and a non-negative premium. |
| `treaty_terms` | `reinsurance_treaty` | A treaty names its type; proportional shares stay within [0, 1]. |

| Metric | Measures |
|---|---|
| `claim_settlement_time` | lead_time over `claim`: Days from the claim record being created to it being paid, closed, denied or withdrawn (record creation time, not notified_at). |
| `open_claims` | count over `claim`: Claims not yet paid, closed, denied or withdrawn, by state, cause, loss event and adjuster. By adjuster, event, loss_cause, state. Certified. |
| `outstanding_reserves` | sum over `claim`: Case reserves on open claims, by loss event and policy. By event, policy. |
| `written_premium` | sum over `insurance_policy`: Annual premium of policies in force, by line of business and intermediary. By intermediary, line. Certified. |

| Action type | Risk | Roles | Does |
|---|---|---|---|
| `cancel_insurance_policy` | high | underwriter, admin | Cancel a bound or in-force policy, recording why. |
| `refer_claim_to_investigation` | medium | any writer | Refer an open claim to the special investigations unit. |
| `settle_claim` | high | claims_handler, admin | Pay an approved claim and release its reserve. |

**The deliberate violation.** claim:clm-20260006 was taken by phone without a loss date: the one deliberate violation (shape claim_complete, constraint claim_complete/loss_date, min_count 1 on prop.loss_date). Everything else in the sample conforms. Installed with its sample into a workspace of its own:

```text
$ onto -w pk-insurance pack install insurance --sample
installed insurance 1.0.0 66 schema changes · schema v9
  sample: 37 entities, 84 links
$ onto -w pk-insurance reason run --samples 0
17 facts derived from 176 asserted (52 rules · 3 rounds · fixpoint reached)
…
$ onto -w pk-insurance shapes validate
violates 17 focus nodes · 1 violations, 0 warnings, 0 info
  severity  shape                    component    focus                    path               message
  violation claim_complete           min_count    claim:clm-20260006       prop.loss_date     Record the date of loss: cover depends on it.
…
```

**Standards it borrows vocabulary from** (no compliance claimed):

- vocabulary inspired by the ACORD insurance data standards (party, policy, coverage, claim)
- lines of business in the style of the Solvency II classification
- reinsurance treaty types as used in common market practice (quota share, surplus, excess of loss, stop loss)

## Energy and utilities (`energy`)

An electric utility's grid: substations, feeders, transformers, switching devices, meters and service points in an electrical topology, territories, outages, crews and condition assessments.

11 object types, 1 interface, 14 link types, 6 rules, 5 shapes, 4 metrics, 3 action types, 25 glossary terms and 32 sample entities. Industry `energy`, version 1.0.0, requires `enterprise`.

| Object type | What it is |
|---|---|
| `condition_assessment` | An inspection or test of one asset and the condition it found. |
| `feeder` | Distribution circuit leaving a substation and serving transformers and service points. (implements `grid_asset`) |
| `generation_unit` | A generator or a distributed energy resource (solar, wind, storage) connected to the grid. (implements `grid_asset`) |
| `grid_outage` | A sustained interruption of supply, from first report to restoration. Planned outages carry planned = true. |
| `service_point` | The point where the grid delivers energy to a premise. Customer data stays in the billing system; only the connection lives here. |
| `service_territory` | An area the utility serves: a region, an operating district or a zone. Territories nest with sub_territory_of. |
| `smart_meter` | Advanced metering infrastructure (AMI) meter. Meters send a last-gasp message when they lose power. (implements `grid_asset`) |
| `substation` | Facility that transforms voltage and switches circuits between transmission and distribution. (implements `grid_asset`) |
| `switching_device` | Breaker, recloser, sectionalizer, fuse or switch that opens and closes a circuit. (implements `grid_asset`) |
| `switching_order` | An approved sequence of switching steps that isolates equipment safely, usually for a planned outage. |
| `transformer` | Distribution or power transformer stepping voltage down towards service points. (implements `grid_asset`) |

Interfaces: `grid_asset` (Physical equipment of the grid: it has an asset tag, a criticality and a maintaining crew).

Link types: `condition_of`, `crew_assigned`, `downstream_of`, `for_outage`, `in_territory`, `interconnected_at`, `isolates`, `maintained_by_crew`, `meter_at`, `originated_at`, `reported_via`, `sub_territory_of`, `surveyed_by`, `tied_with`. Transitive: `downstream_of`, `sub_territory_of`. Symmetric: `tied_with`. Dependencies: `downstream_of`.

| Rule | Form | Derives | On the sample |
|---|---|---|---|
| `territory_rollup` | chain | `within_territory = in_territory > sub_territory_of`: What lies in a territory lies within every territory that contains it. | 14 |
| `responsible_crew` | chain | `responsible_crew = originated_at > maintained_by_crew`: The crew maintaining the asset where an outage started is responsible for it. | 3 |
| `interrupted_service` | horn | `?sp interrupted_by ?o <= ?sp a service_point, ?sp downstream_of ?asset, ?o originated_at ?asset`: A service point supplied through the asset where an outage started is interrupted by that outage. The relation is a record of the event and stays after restoration. | 5 |
| `outage_affecting_life_support` | horn | `?o a outage_affecting_life_support <= ?sp interrupted_by ?o, ?sp prop.life_support true`: An outage that interrupts or interrupted a life-support premise. The class is a record, not a priority flag: it stays after restoration, so read the outage's state to see whether it is still open. | 1 |
| `grid_asset_at_risk` | horn | `?a a grid_asset_at_risk <= ?c condition_of ?a, ?c prop.condition very_poor, ?a prop.criticality critical`: A critical asset whose condition was assessed as very poor is at risk. | 1 |
| `condition_related_outage` | horn | `?o a condition_related_outage <= ?o originated_at ?a, ?c condition_of ?a, ?c prop.condition very_poor`: An outage that started at an asset assessed as very poor is condition related. | 1 |

Derived classes: `condition_related_outage`, `grid_asset_at_risk`, `outage_affecting_life_support`. Derived relations: `interrupted_by`, `responsible_crew`, `within_territory`.

| Shape | Target | Checks |
|---|---|---|
| `assessment_complete` | `condition_assessment` | A condition assessment names the asset, the condition found and when. |
| `outage_record` | `grid_outage` | An outage names its origin and cause and counts the customers it affects. |
| `service_point_connected` | `service_point` | Every service point is connected to the grid and lies in one territory. |
| `smart_meter_installed` | `smart_meter` | Every meter measures exactly one service point. |
| `transformer_rating` | `transformer` | A transformer has a plausible nameplate rating and an upstream supply. |

| Metric | Measures |
|---|---|
| `customers_interrupted` | sum over `grid_outage`: Customers affected by outages that are still open. By cause, crew. |
| `generation_capacity_mw` | sum over `generation_unit`: Nameplate generation capacity in MW. By fuel, state, territory. |
| `open_grid_outages` | count over `grid_outage`: Outages not yet restored or closed. By cause, crew, origin, state. Certified. |
| `outage_restoration_days` | lead_time over `grid_outage`: Days from an outage being recorded to being restored or closed. |

| Action type | Risk | Roles | Does |
|---|---|---|---|
| `dispatch_outage_crew` | low | any writer | Dispatch a field crew to an open outage. |
| `restore_outage` | medium | any writer | Record that supply is restored for an outage. |
| `retire_transformer` | high | admin | Retire a transformer from service (asset register change). |

**The deliberate violation.** smart_meter:m-stock-0001 is a meter still in stock with no service point: the one deliberate violation (shape smart_meter_installed, min_count 1 on meter_at). Everything else in the sample conforms. Installed with its sample into a workspace of its own:

```text
$ onto -w pk-energy pack install energy --sample
installed energy 1.0.0 69 schema changes · schema v10
  sample: 32 entities, 50 links
$ onto -w pk-energy reason run --samples 0
58 facts derived from 171 asserted (58 rules · 3 rounds · fixpoint reached)
…
$ onto -w pk-energy shapes validate
violates 19 focus nodes · 1 violations, 0 warnings, 0 info
  severity  shape                    component    focus                    path               message
  violation smart_meter_installed    min_count    smart_meter:m-stock-0001 meter_at           A meter must be installed at one service point.
…
```

**Standards it borrows vocabulary from** (no compliance claimed):

- vocabulary inspired by the IEC 61968/61970 Common Information Model (CIM)
- asset-condition terms inspired by ISO 55000 asset management
- interruption terms inspired by IEEE 1366 distribution reliability indices

## Telecommunications (`telecom`)

Network resources (sites, elements, radio cells, transport links), the services they realize, subscriber accounts and SIM cards, alarms, trouble tickets and maintenance windows. Alarm impact climbs the transitive uplink hierarchy.

10 object types, 1 interface, 14 link types, 6 rules, 5 shapes, 4 metrics, 3 action types, 23 glossary terms and 32 sample entities. Industry `telecom`, version 1.0.0, requires `enterprise`.

| Object type | What it is |
|---|---|
| `maintenance_window` | Planned work on network resources during an agreed window. |
| `network_alarm` | An alarm raised by a network resource, from raise to clear. |
| `network_element` | A managed piece of network equipment: router, switch, optical line terminal, baseband unit or firewall. (implements `network_resource`) |
| `network_service` | A customer-facing service (mobile data, broadband, enterprise VPN, SIP trunk) realized by network resources. |
| `network_site` | A physical location that houses network equipment: a cell tower, a rooftop, a central office or a data centre. |
| `radio_cell` | A radio sector served by a baseband unit, identified by its physical cell identity (PCI). (implements `network_resource`) |
| `sim_card` | A physical SIM or eSIM profile bound to a subscriber account, identified by its ICCID. |
| `subscriber_account` | A billing account that subscribes to services: a consumer, a business or a wholesale partner. |
| `transport_link` | A fibre, microwave, copper or satellite link between two network elements (backhaul and core transport). (implements `network_resource`) |
| `trouble_ticket` | A customer-reported or network-detected problem with a service. A subtype of issue, so it can be assigned and blocked. (extends `issue`) |

Interfaces: `network_resource` (Anything that carries traffic and can raise alarms: network elements, radio cells and transport links).

Link types: `alarm_on`, `caused_by_alarm`, `cell_of`, `correlates_with`, `housed_at`, `raised_by_account`, `realized_by`, `route_diverse_with`, `sim_of`, `subscribes_to`, `terminates_on`, `ticket_for_service`, `uplinks_to`, `window_covers`. Transitive: `uplinks_to`. Symmetric: `correlates_with`, `route_diverse_with`. Dependencies: `cell_of`, `realized_by`, `uplinks_to`.

| Rule | Form | Derives | On the sample |
|---|---|---|---|
| `alarm_service_impact` | chain | `alarm_impacts = alarm_on > realizes`: An alarm on a resource impacts the services that resource realizes. alarm_impacts is a record and stays after the alarm clears; the risk classification checks the alarm's state. | 3 |
| `upstream_alarm_impact` | horn | `?a alarm_impacts ?s <= ?s realized_by ?e, ?e uplinks_to ?up, ?a alarm_on ?up`: An alarm on an element impacts services realized by any element that uplinks to it (directly or through the hierarchy). | 4 |
| `cell_realization` | horn | `?s realized_by ?e <= ?s realized_by ?c, ?c cell_of ?e`: A service that runs on a radio cell also runs on the baseband unit serving that cell, so alarms on the unit and everything above it reach the service. | 1 |
| `affected_subscribers` | chain | `alarm_affects_account = alarm_impacts > subscribers`: An alarm that impacts a service affects the accounts subscribed to it. | 9 |
| `sla_breach_risk` | horn | `?s a sla_breach_risk <= ?a alarm_impacts ?s, ?a prop.perceived_severity critical, ?a state raised, ?s prop.sla_tier platinum`: A platinum service impacted by a raised (not yet acknowledged or cleared) critical alarm is at risk of breaching its SLA. | 1 |
| `sim_swap_risk` | horn | `?acct a sim_swap_risk <= ?sim sim_of ?acct, ?sim state swapped, ?acct prop.port_out_pending true`: An account whose SIM was just swapped and that has a port-out pending shows the SIM-swap fraud pattern. | 1 |

Derived classes: `sim_swap_risk`, `sla_breach_risk`. Derived relations: `alarm_affects_account`, `alarm_impacts`.

| Shape | Target | Checks |
|---|---|---|
| `alarm_source` | `network_alarm` | An alarm names the resource that raised it and its perceived severity. |
| `radio_cell_placement` | `radio_cell` | A radio cell is housed at one site, served by one baseband unit and has a valid PCI. |
| `service_realization` | `network_service` | A service runs on at least one resource and has an SLA tier. |
| `sim_binding` | `sim_card` | A SIM card is bound to exactly one account and has a well-formed ICCID. |
| `transport_endpoints` | `transport_link` | A transport link has exactly two ends. |

| Metric | Measures |
|---|---|
| `active_network_alarms` | count over `network_alarm`: Alarms not yet cleared. By resource, severity, site, state. Certified. |
| `open_trouble_tickets` | count over `trouble_ticket`: Trouble tickets not yet resolved or closed. By assignee, channel, priority, service. Certified. |
| `subscriber_arpu` | avg over `subscriber_account`: Average monthly revenue per active subscriber account. By segment. |
| `ticket_restore_time` | lead_time over `trouble_ticket`: Days from a trouble ticket being opened to being resolved or closed. |

| Action type | Risk | Roles | Does |
|---|---|---|---|
| `acknowledge_alarm` | low | any writer | Acknowledge a raised alarm (someone in the NOC is on it). |
| `open_trouble_ticket` | low | any writer | Open a trouble ticket for a service, optionally on behalf of a subscriber account. |
| `suspend_service` | high | admin | Suspend an active customer-facing service. |

**The deliberate violation.** transport_link:mw-hop-77 is a microwave hop recorded with only one end: the one deliberate violation (shape transport_endpoints, min_count 2 on terminates_on). Everything else in the sample conforms. Installed with its sample into a workspace of its own:

```text
$ onto -w pk-telecom pack install telecom --sample
installed telecom 1.0.0 66 schema changes · schema v11
  sample: 32 entities, 49 links
$ onto -w pk-telecom reason run --samples 0
39 facts derived from 175 asserted (64 rules · 4 rounds · fixpoint reached)
…
$ onto -w pk-telecom shapes validate
violates 16 focus nodes · 1 violations, 0 warnings, 0 info
  severity  shape                    component    focus                    path               message
  violation transport_endpoints      min_count    transport_link:mw-hop-77 terminates_on      A transport link must terminate on two network elements.
…
```

**Standards it borrows vocabulary from** (no compliance claimed):

- resource and service vocabulary inspired by the TM Forum Information Framework (SID)
- alarm severity terms inspired by ITU-T X.733 alarm reporting
- radio access terms inspired by 3GPP (LTE and NR cells, physical cell identity)

## SaaS: product, customers, subscriptions and support (`saas`)

A software business: products, features and pricing plans, customer accounts and users, subscriptions and invoices, support tickets, NPS responses and feature requests.

10 object types, 0 interfaces, 16 link types, 6 rules, 5 shapes, 4 metrics, 3 action types, 22 glossary terms and 41 sample entities. Industry `software`, version 1.0.0, requires `enterprise`.

| Object type | What it is |
|---|---|
| `customer_account` | An organization that buys the product. Subsidiaries roll up to their parent account. |
| `feature_request` | Something customers ask the product to do. |
| `nps_response` | One answer to the Net Promoter Score survey. |
| `pricing_plan` | A tier of a product with a price and the features it includes. |
| `product_feature` | A capability of a product, usually behind a feature flag. |
| `product_user` | A person at a customer account who signs in to the product (not a member of this workspace). |
| `software_product` | A product the company sells as a service. |
| `subscription` | An account's paid (or trial) use of a plan for a number of seats. |
| `subscription_invoice` | A bill for one period of a subscription. |
| `support_ticket` | A customer's request for help. A subtype of issue, so it can be assigned, blocked and resolved like any issue (it keeps issue's triaged, blocked and resolved states, which enterprise's assign, block and resolve actions set). (extends `issue`) |

Link types: `billed_for`, `concerns_feature`, `escalated_as`, `feature_of`, `includes_feature`, `managed_by_csm`, `nps_from`, `on_plan`, `plan_of`, `raised_by_user`, `requested_by_account`, `requires_feature`, `rolls_up_to`, `subscription_of`, `ticket_from`, `user_of`. Transitive: `requires_feature`, `rolls_up_to`. Dependencies: `requires_feature`.

| Rule | Form | Derives | On the sample |
|---|---|---|---|
| `feature_entitlement` | chain | `entitled_to = account_subscriptions > on_plan > includes_feature`: An account is entitled to the features its subscriptions' plans include. | 6 |
| `ticket_csm` | chain | `csm_for_ticket = ticket_from > managed_by_csm`: A support ticket goes to the customer success manager of the account it comes from. | 2 |
| `payment_risk` | horn | `?a a payment_risk_account <= ?i billed_for ?s, ?i state uncollectible, ?s subscription_of ?a`: An account with an uncollectible invoice on one of its subscriptions is a payment risk. | 1 |
| `detractor_account` | horn | `?a a detractor_account <= ?r nps_from ?u, ?r prop.nps_category detractor, ?u user_of ?a`: An account with a user who answered the NPS survey as a detractor is a detractor account. | 1 |
| `churn_risk` | horn | `?a a churn_risk_account <= ?a a payment_risk_account, ?a a detractor_account`: An account that is both a payment risk and a detractor is a churn risk. | 1 |
| `candidate_plan` | horn | `?a candidate_plan ?p <= ?r requested_by_account ?a, ?r concerns_feature ?f, ?p includes_feature ?f, ?p state active`: An active plan that includes a feature an account requested is a candidate plan for it. Rules have no negation, so this does not check whether the account already has the feature. | 5 |

Derived classes: `churn_risk_account`, `detractor_account`, `payment_risk_account`. Derived relations: `candidate_plan`, `csm_for_ticket`, `entitled_to`.

| Shape | Target | Checks |
|---|---|---|
| `invoice_terms` | `subscription_invoice` | An invoice bills one subscription, has a non-negative amount and an ISO 4217 currency. |
| `nps_scale` | `nps_response` | An NPS answer comes from one user and scores 0 to 10. |
| `product_user_account` | `product_user` | Every product user belongs to exactly one customer account. |
| `subscription_terms` | `subscription` | A subscription belongs to one account, is on one plan and has at least one seat. |
| `ticket_triage` | `support_ticket` | A support ticket names the account it comes from and its severity. |

| Metric | Measures |
|---|---|
| `monthly_recurring_revenue` | sum over `subscription`: Monthly recurring revenue of active and past-due subscriptions. By account, plan, product, state. Certified. |
| `open_support_tickets` | count over `support_ticket`: Support tickets not yet resolved or closed. By account, assignee, feature, severity. Certified. |
| `seats_under_contract` | sum over `subscription`: Seats on active subscriptions. By plan. |
| `support_resolution_time` | lead_time over `support_ticket`: Days from a support ticket being opened to being resolved or closed. |

| Action type | Risk | Roles | Does |
|---|---|---|---|
| `cancel_subscription` | high | any writer | Cancel a subscription and stop it from renewing. |
| `change_subscription_plan` | medium | any writer | Move a subscription to another pricing plan (upgrade or downgrade). |
| `escalate_support_ticket` | low | any writer | Escalate a support ticket to engineering as a new issue in a project. |

**The deliberate violation.** nps_response:nps-0007 records a score of 11 from a broken survey widget: the one deliberate violation (shape nps_scale, max_value 10 on prop.nps_score). Everything else in the sample conforms. Installed with its sample into a workspace of its own:

```text
$ onto -w pk-saas pack install saas --sample
installed saas 1.0.0 66 schema changes · schema v12
  sample: 41 entities, 62 links
$ onto -w pk-saas reason run --samples 0
26 facts derived from 197 asserted (70 rules · 3 rounds · fixpoint reached)
…
$ onto -w pk-saas shapes validate
violates 19 focus nodes · 1 violations, 0 warnings, 0 info
  severity  shape                    component    focus                    path               message
  violation nps_scale                max_value    nps_response:nps-0007    prop.nps_score     NPS scores run from 0 to 10. (value 11)
…
```

**Standards it borrows vocabulary from** (no compliance claimed):

- organization and offer vocabulary inspired by schema.org (Organization, Offer, SoftwareApplication)
- support ticket terms inspired by ITIL 4 incident and service request management
- feature flag terms inspired by the OpenFeature specification
- survey categories borrowed from the Net Promoter Score method (promoter 9-10, passive 7-8, detractor 0-6)
