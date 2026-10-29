(ns cmh.kernel.state
  "Journal-backed state. The journal (journal.jsonl) is the single source of
  truth; in-memory state is always (reduce apply-event empty-state events),
  both live and on replay, so a fresh process reproduces the same state."
  (:require [clojure.data.json :as json]
            [clojure.java.io :as io]
            [clojure.string :as str])
  (:import [java.time Instant]))

(def empty-state {:defs {} :knowledge {} :order [] :tasks []})

(def def-fields
  "The fields of a proposal that a definition version record keeps."
  [:name :kind :version :source :description :tests :input_schema])

(defn trace [k & xs] ((:trace k) (apply str xs)))

(defn- set-status [st name version status]
  (assoc-in st [:defs name :versions version :status] status))

(defn apply-event
  "Pure state transition for one journal event."
  [st {:keys [type name version] :as ev}]
  (case type
    "def_proposed"
    (-> st
        (assoc-in [:defs name :kind] (:kind ev))
        (assoc-in [:defs name :versions version]
                  (-> (select-keys ev (conj def-fields :ts))
                      (assoc :status "proposed"))))

    "def_rejected"
    (-> st (set-status name version "rejected")
        (assoc-in [:defs name :versions version :error] (:error ev)))

    "def_trial"
    (-> st (set-status name version "trial")
        (assoc-in [:defs name :live] version)
        (assoc-in [:defs name :uses] 0))

    "def_use"
    (if (and (:ok ev)
             (= version (get-in st [:defs name :live]))
             (= "trial" (get-in st [:defs name :versions version :status])))
      (update-in st [:defs name :uses] (fnil inc 0))
      st)

    "def_kept"
    (-> st (set-status name version "kept")
        (assoc-in [:defs name :live] version)
        (assoc-in [:defs name :kept] version)
        (assoc-in [:defs name :uses] 0))

    "def_reverted"
    (let [d (get-in st [:defs name])]
      (-> st (set-status name version "reverted")
          (assoc-in [:defs name :versions version :error] (:error ev))
          (cond-> (= version (:live d)) (assoc-in [:defs name :live] (:kept d)))
          (assoc-in [:defs name :uses] 0)))

    "lesson_added"
    (-> st
        (assoc-in [:knowledge (:id ev)]
                  {:id (:id ev) :kind (or (:kind ev) "lesson") :text (:text ev)
                   :source (:source ev) :score 0 :retired false :ts (:ts ev)})
        (update :order conj (:id ev)))

    "lesson_scored" (assoc-in st [:knowledge (:id ev) :score] (:score ev))
    "lesson_retired" (assoc-in st [:knowledge (:id ev) :retired] true)

    "task_started"
    (update st :tasks conj {:id (:id ev) :task (:task ev) :lessons (:lessons ev)})

    "task_finished"
    (update st :tasks
            (fn [ts] (mapv #(if (= (:id %) (:id ev))
                              (assoc % :success (:success ev) :error (:error ev))
                              %)
                           ts)))
    st))

(defn read-journal [^java.io.File f]
  (if-not (.exists f)
    []
    (with-open [r (io/reader f)]
      (->> (line-seq r)
           (remove str/blank?)
           (keep #(try (json/read-str % :key-fn keyword)
                       (catch Exception _ nil))) ; torn final line after a crash
           vec))))

(defn replay [events] (reduce apply-event empty-state events))

(defn emit!
  "Append one event to the journal and apply it. The event is round-tripped
  through JSON first so live state equals replayed state exactly."
  [k ev]
  (locking (:state k)
    (let [line (json/write-str (assoc ev :ts (str (Instant/now))))
          ev' (json/read-str line :key-fn keyword)]
      (with-open [w (io/writer (:journal k) :append true)]
        (.write w (str line "\n")))
      (swap! (:state k) apply-event ev')
      ev')))

;; ---- queries ---------------------------------------------------------------

(defn live-record [st name]
  (let [d (get-in st [:defs name])]
    (when-let [v (:live d)] (get-in d [:versions v]))))

(defn latest-version
  "Highest version number proposed for name, or nil when there is none."
  [st name]
  (some->> (keys (get-in st [:defs name :versions])) seq (apply max)))

(defn next-version [st name] (inc (or (latest-version st name) 0)))

(defn live-defs
  "Live records of one kind, sorted by name."
  [st kind]
  (->> (keys (:defs st)) sort
       (keep #(live-record st %))
       (filter #(= kind (:kind %)))))

(defn entries [st] (map (:knowledge st) (:order st)))
(defn lessons [st] (filter #(= "lesson" (:kind %)) (entries st)))
(defn active-lessons [st] (remove :retired (lessons st)))
(defn verdicts [st] (filter #(= "verdict" (:kind %)) (entries st)))
