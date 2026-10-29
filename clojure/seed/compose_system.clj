;; Hook: (compose-system notes lessons) -> string
;; notes and lessons are vectors of strings. The kernel prepends the base prompt.
(require '[clojure.string :as str])

(defn- section [title items]
  (when (seq items)
    (str "## " title "\n" (str/join "\n" (map #(str "- " %) items)))))

(defn compose-system [notes lessons]
  (str/join "\n\n" (remove nil? [(section "Notes" notes)
                                 (section "Lessons" lessons)])))
