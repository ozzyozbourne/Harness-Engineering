;; Hook: (select-lessons task lessons) -> vector of lesson ids
;; lessons are maps with :id :text :score.
(require '[clojure.string :as str] '[clojure.set :as set])

(defn- words [s]
  (set (filter #(>= (count %) 4)
               (map str/lower-case (re-seq #"[A-Za-z]+" (str s))))))

(defn select-lessons [task lessons]
  (let [tw (words task)
        ranked (sort-by #(- (:score %)) lessons)
        matching (filter #(seq (set/intersection tw (words (:text %)))) ranked)]
    (mapv :id (if (seq matching) (take 8 matching) (take 3 ranked)))))
